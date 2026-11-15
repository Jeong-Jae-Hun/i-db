# i-db

`db` is a terminal database editor for PostgreSQL and Redis. It reads an existing DataGrip project (`.idea/dataSources.xml` and friends) as its connection list and takes passwords from a `.env` file next to it, so there is no second place to maintain connections. One binary gives you a full-screen TUI, a scriptable CLI and a read-only MCP server for AI agents.

The behavior is specified rule by rule in [SPEC.md](SPEC.md). The UI text and the spec are currently written in Korean.

## Features

- Connection tree grouped like DataGrip, with schemas, tables, views, functions and Redis key namespaces
- SQL editor with syntax highlighting, completion, live templates (`sel`, `ins`, `upd`, ...), formatting, `:name`/`$1` parameters, go-to-definition
- Results grid: sort (`s` or header click), filter with `/` (`abc`, `!abc`, `col:abc`, `col=v`, `col>v`, `col=null`), copy as TSV/CSV/JSON/INSERT/UPDATE/`IN (...)`, charts, JSON tree, cell aggregates, result pinning and diff
- In-place editing of single-table results (preview SQL before submit, undo of the last submit), paste from spreadsheets, CSV import
- Server-side table view with a DataGrip-style WHERE / ORDER BY bar (column completion, `/` short filters), paging, exact count; follow foreign keys in both directions
- Command palette (`Ctrl+P`) that runs any action or jumps to any table across connections, recently opened tables first
- EXPLAIN / EXPLAIN ANALYZE as a plan tree, activity/locks/table sizes, cancel or terminate backends
- Transactions in Auto or Manual mode, full-schema data search, schema diff between connections, run a statement on another connection and diff the results
- Redis: key browser, value editor for every type, TTL/rename, monitor (INFO, SLOWLOG, big keys), pattern bulk delete
- `queries/` folder integration: files are attached to connections via DataGrip's `data_source_mapping.xml`, auto-save, session restore
- Optional client-side decryption view and `enc('...')` expansion for AES-CBC values stored as `<iv hex>^|^<ciphertext hex>` (pgcrypto `encrypt_iv` compatible). Keys come from `.env` (`<GROUP>_AES_KEY`, `<GROUP>_AES_IV`); nothing is installed in your database. Without a key/IV, `enc(...)` is sent as a call to an existing `staticEncrypt(...)` function, so it only works if your database already has one
- Read-only sources next to the databases: OpenSearch, Loki, Prometheus, Grafana (Loki/Prometheus data sources) and DynamoDB in the same tree, editor and results grid (see Sources)
- Optional "user lookup" that runs a configurable set of read-only queries across several databases from one email/uid/id/nickname (see [examples/user360.json](examples/user360.json))

## Install

### Release archives

Each `v*` tag publishes `db_<os>_<arch>.tar.gz` for `darwin` and `linux` on `amd64` and `arm64`, plus `checksums.txt`, on the [Releases](https://github.com/Jeong-Jae-Hun/i-db/releases) page. The archive contains a single binary named `db`.

```sh
os=$(uname -s | tr A-Z a-z); arch=$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')
curl -fsSL "https://github.com/Jeong-Jae-Hun/i-db/releases/latest/download/db_${os}_${arch}.tar.gz" | tar -xz -C ~/.local/bin db
db -version
```

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

### Sources

OpenSearch, Loki, Prometheus, Grafana and DynamoDB connections are read-only and live only in `.dbtui/connections.json` (DataGrip sources are unchanged). Fields are `url` (DynamoDB: optional endpoint), `auth` (`basic`, `sigv4` for OpenSearch, `token` for Grafana, or none), `user`, and `region`/`profile` for SigV4 and DynamoDB. Secrets go to `.env` as `OS_<NAME>_PASSWORD`, `LOKI_<NAME>_PASSWORD`, `PROM_<NAME>_PASSWORD` or `GRAFANA_<NAME>_TOKEN`; AWS credentials are never stored and come from `profile` or the standard chain.

```json
[{"uuid":"…","name":"search-stg","kind":"opensearch","url":"https://search.stg.example.internal","auth":"sigv4","region":"ap-northeast-2","profile":"stg"},
 {"uuid":"…","name":"logs-stg","kind":"loki","url":"http://loki.stg.internal:3100"},
 {"uuid":"…","name":"orders","kind":"dynamodb","region":"ap-northeast-2"}]
```

In the connection form the kind field cycles through all seven kinds and shows only the fields that kind needs; pasting an `https://user:pw@host:9200` URL fills url, user and password. In the tree, these connections expand lazily (indices and mapping fields, Loki labels and values, Prometheus metrics, Grafana data sources, DynamoDB tables and keys) with details dimmed on the right. `Enter` previews an index, label value, metric or table in a result tab, `l` expands, `r` reloads. `Ctrl+R` sends the editor text as the source's own query language (`GET idx/_search` + JSON body or `SELECT …` for OpenSearch, LogQL, PromQL, PartiQL `SELECT`). Anything that looks like a write is refused before sending (`읽기 전용 저장소라 보내지 않음`), and expensive queries (DynamoDB full scan, OpenSearch `size` over 10,000, Loki/Prometheus ranges over 7 days) ask `비싼 질의: y/N` first. Loki/Prometheus/Grafana tabs show the range (`최근 1h`) above the editor; `Ctrl+]` cycles 15m, 1h, 6h, 24h, 7d per tab. Results are ordinary grids, so `c` charts metric series. Table view, editing and transactions do not apply.

```sh
db conn add logs-stg --kind loki --url http://loki.stg.internal:3100
printf '%s' "$GRAFANA_TOKEN" | db conn add graf --kind grafana --url https://grafana.example.com --password-stdin
db tables -c search-stg
db query -c search-stg 'GET logs-*/_search
{"query":{"match":{"status":500}},"size":20}'
db logs -c logs-stg '{app="api"} |= "error"' --since 6h
db metrics -c prom 'sum by (status) (rate(http_requests_total[5m]))' --since 24h --until 1h
db query -c orders 'SELECT * FROM "orders"' --force   # full scan needs --force (exit 3 without)
```

`--max-rows` defaults to 1,000 for sources. Prod sources need `--prod` like everything else; write-looking queries and unconfirmed expensive queries exit with 3. `describe`, `ddl`, `explain`, `redis`, `search`, `schemas` and `schema-diff` are usage errors on these connections.

## TUI

Press `F1` for every key binding, or `Ctrl+P` for the command palette. Some common ones:

| Key | Action |
|---|---|
| `Ctrl+R` / `F5` | run selection, or the statement under the cursor |
| `Ctrl+E` / `Alt+E` | EXPLAIN plan tree / EXPLAIN ANALYZE |
| `Ctrl+Space` | completion |
| `Ctrl+N` | find objects (tables, columns, functions, Redis keys) |
| `Ctrl+O` / `Ctrl+S` | open / save a file under `queries/` |
| `F9` / `F10` / `F11` | Auto/Manual transactions / COMMIT / ROLLBACK |
| `Ctrl+L` | unlock writes on a prod connection for a few minutes |
| `Ctrl+]` | Loki/Prometheus range 15m · 1h · 6h · 24h · 7d (per tab) |
| `Ctrl+H` | history |
| tree `Enter` on a table | table view (`w`·`/` WHERE, `O` ORDER BY, `[` `]` pages) |
| tree `N` / `E` | add / edit a connection (see Connections) |

On macOS, in terminals that speak the kitty keyboard protocol (Ghostty, kitty, WezTerm, iTerm2), the same actions are bound to ⌘ keys. See SPEC R23 and R47.

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
db conn add app-stg2 --url 'postgres://app_ro:pw@db.stg.example.internal:5432/app'
```

Output formats are `table`, `tsv`, `csv` and `json` (table on a terminal, tsv otherwise). Reads run inside a server-side READ ONLY transaction. Exit codes: 0 success, 1 execution error, 2 usage, 3 refused by a safety check. CLI runs are not written to history.

## MCP

`db mcp` starts a read-only MCP server over stdio with the same project settings. Prod connections are only exposed with `--allow-prod`.

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
| `XDG_STATE_HOME` | where history, session, parameter values and undo log live (default `~/.local/state/db/`) |

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
