# db serve 프로토콜

`db serve -p <프로젝트>`는 표준 입출력으로 엔진 요청을 받는다. 포트를 열지 않는다. Mac 의 `db`는 `ssh <호스트> <db> serve -p <프로젝트>`를 띄워 WSL 에 있는 접속을 쓴다. 접속 정의·`.env` 비밀번호·VPN 경로는 serve 쪽에만 있고, 안전장치(읽기 전용, prod 확인·잠금, 위험 문장, 가리기)도 serve 쪽이 정한다. 클라이언트는 사용자가 확인했다는 표시만 보낸다.

## 줄

한 줄에 JSON 하나다. 요청은 `{"id":N,"op":"…",…}`이고 응답은 `{"id":N,"ok":true,"result":…}` 또는 `{"id":N,"ok":false,"code":3,"error":"…","reason":"…"}`이다. id 는 클라이언트가 고르는 양의 정수이고 진행 중인 요청끼리 겹치면 안 된다. 요청은 동시에 돌고 응답은 끝난 순서로 온다. 모르는 필드는 무시한다.

code 는 CLI 종료 코드와 같다. 1 실행 오류, 2 요청·사용법 오류, 3 안전장치 거절이다. code 3 의 reason 은 `prod_locked`(R54, 확인 창 없이 거절), `confirm_prod`(R6, 확인 후 `confirm_prod:true`로 다시 보냄), `confirm_danger`(R25, `confirm_danger:true`), `confirm`(백엔드 신호·패턴 일괄 삭제처럼 환경과 상관없이 늘 묻는 것, `confirm:true`), `confirm_analyze`(읽기 전용이 아닌 문장의 EXPLAIN ANALYZE, R9.2, `confirm_analyze:true`)이다. Redis `KEYS`는 접속하기 전에 `keys`로 거절한다(확인으로 풀리지 않음, SCAN 을 쓴다). 저장소 op 는 `read_only`(쓰기로 보이는 질의, 다시 보낼 길 없음), `confirm_expensive`(전체 스캔·7일 넘는 범위 같은 비싼 질의 R60.2, 확인 후 `force:true`), `billed`(요청마다 요금이 붙는 AWS 조회 R72, serve 쪽 `DB_ALLOW_BILLED=1` 없이는 확인으로도 풀리지 않음)를 쓴다. 접속 편집 op 는 `remote_conn_edit`(serve 는 접속 파일을 쓰지 않고 비밀번호를 받지 않음, 아래 「접속 편집」)를 쓴다. error 글은 그대로 확인 창에 보일 수 있는 질문(`prod(app-prod)에 제출 2건: y/N`)이다. 편집기 실행이 실패했는데 트랜잭션이 열려 있으면 code 1 에 reason `in_tx`가 붙는다.

어느 메시지에도 비밀번호·AES 키를 싣지 않는다. 오류 글은 serve 가 그 접속의 비밀번호와 모든 AES 키·IV 를 지우고 제어 문자를 바꾼 것이다. 단, staticEncrypt 암호문은 형식상 앞에 IV 를 싣는다(`IV^|^…`).

## 인사와 취소

첫 요청은 `{"id":1,"op":"hello","version":1}`이어야 한다. 답은 `{"version":1,"server":"db-rs"}`다. 그 전 요청과 다른 판은 code 2 다.

`{"id":9,"op":"cancel","target":5}`는 진행 중인 5 를 멈춘다. 5 는 code 1 `canceled`로 끝나고 9 의 답은 `{"canceled":true}`다(이미 끝났으면 false). PostgreSQL 실행을 멈추면 세션 연결이 닫히고 다음 실행은 한 번 R3.6 세션 유실 오류를 낸다. 단, `cli`는 serve 안에서 막는 작업이라 응답만 끊기고 명령은 `--timeout`까지 돈다.

판 1 은 op 를 더하기만 한다. 옛 클라이언트는 새 op 를 쓰지 않으면 그대로 돌고, 새 클라이언트가 옛 serve 에 새 op 를 보내면 code 2 `bad request: unknown variant …`다.

입력이 닫히면 serve 는 남은 요청을 60초까지 기다려 답한 뒤 멈추고, 세션을 닫아 열린 트랜잭션을 서버가 되돌리게 한 뒤 끝난다.

## op

| op | 필드 | result |
| --- | --- | --- |
| `project` | | `{dir, conns:[{name,group,folder,kind,env,host,db,source,uuid,region?,profile?}], warnings}`(region·profile 은 빈 값이면 없다) |
| `schemas` | conn | `["public",…]` |
| `tables` | conn, schema(기본 public) | `["t",…]` |
| `describe`·`ddl` | conn, schema, table(`스키마.테이블` 꼴도) | ResultSet · CREATE 글 |
| `search` | conn, schema, text | ResultSet(건너뛴 테이블은 note) |
| `run` | conn, sql(문장 하나), params{이름:값}, confirm_prod, confirm_danger | ResultSet(in_tx, enc() 알림은 note) |
| `explain` | run 필드 + analyze, buffers, format, confirm_analyze | ResultSet. 판정은 감싸기 전 문장으로 한다: ANALYZE 면 속 문장의 쓰기·위험이 그대로이고, 읽기 전용이 아니면 `confirm_analyze`(R9.2) → prod `confirm_prod` → `confirm_danger` 순으로 묻는다. ANALYZE 없는 EXPLAIN 은 묻지 않는다 |
| `table_page` | conn, schema, table, where, order_by, offset, limit(기본 200, 상한 1,000), count, estimate | `{result, sql, count?, estimate?}` |
| `tx_begin`·`tx_rollback`·`tx_state` | conn | `{in_tx, text}` |
| `tx_commit` | conn, confirm_prod | `{in_tx, text}` |
| `prod_lock` | conn, unlock(true 열기·false 잠그기·없으면 보기) | `{conn, prod, locked, remaining_secs, unlock_min}` |
| `redis_scan` | conn, pattern(기본 `*`) | `{keys, more}`(1,000개까지, 정렬) |
| `redis_value` | conn, key | ResultSet(머리 글은 text, 컬렉션은 표) |
| `redis_run` | conn, args, confirm_prod, confirm_danger | ResultSet |
| `cli` | args, input(`{"text":…}`·`{"error":…}`), tty | `{stdout, stderr, code}` |
| `databases` | conn | `["postgres",…]`(R27.1) |
| `switch_db` | conn, db(빈 글이면 프로젝트 DB) | SessionState `{conn, db, search_path, in_tx}`. 열린 트랜잭션이면 code 2 |
| `set_search_path` | conn, schema | SessionState(R27.2, 세션이 다시 열리거나 트랜잭션이 끝나면 다시 보냄) |
| `activity`·`locks`·`table_sizes` | conn | ResultSet(R22.1, 쿼리 글 열의 비밀번호 문장은 가림) |
| `signal_backend` | conn, pid, terminate, confirm, confirm_prod | `{}`(pg_cancel_backend·pg_terminate_backend) |
| `foreign_keys` | conn, schema, table, incoming | `[{name, table, cols, ref, ref_cols}]`(R36) |
| `resolve` | conn, name | `{kind, schema, name}`(R26, 못 찾으면 `definition not found: …`) |
| `function_def` | conn, schema, name | 정의 글 |
| `catalog` | conn | `{schemas:[{name, tables:[{name, view, columns:[{name,type}]}]}], functions}` |
| `enum_labels` | conn, types | `{타입: [값…]}`(enum 이 아니면 빈 목록) |
| `estimate_rows` | conn, sql, params | `{rows}`(R39, UPDATE·DELETE 하나, 실행하지 않음) |
| `apply_changes` | conn, changes(ChangeSet), confirm_prod | `{result, applied, sql}`(R15.2) |
| `undo_changes` | conn, changes, applied, confirm_prod | `{result, applied, sql}`(R50) |
| `changes_sql` | changes, applied? | `{changes, sql}`(미리 보기. applied 가 있으면 되돌리기 묶음) |
| `import_csv` | conn, schema, table, columns, rows, confirm_prod | `{rows}`(COPY 한 번, 10만 행·50MiB 상한) |
| `linked` | conn, column, value, table | `{result, failed}`(R73) |
| `schema_diff` | conn, to, schema, to_schema | ResultSet |
| `user360` | query | `[{title, conn, result?, error?}]`(R51) |
| `redis_raw_value` | conn, key | `{key, type, ttl_secs, string, fields, items, members, zmembers, truncated}` |
| `redis_edit` | conn, edit, confirm_prod, confirm_danger | `{results, key, status}` |
| `redis_info`·`redis_slowlog`(n=50)·`redis_big_keys`(sample=1000, top=50) | conn | ResultSet(R34.1) |
| `redis_pattern_keys` | conn, pattern | `{keys, truncated}`(R42, 100,000개까지) |
| `redis_unlink` | conn, keys, confirm, confirm_prod | `{rows}`(지운 수) |
| `source_children` | conn, path(기본 맨 위 "") | `[{name, path, info, has_children, openable, warn}]`(R60~R68) |
| `source_open` | conn, path, limit(0 이면 Loki 100줄·나머지 50행, 상한 1,000) | ResultSet(트리 Enter 미리 보기) |
| `source_run` | conn, text, since_ns, until_ns, limit(기본 1,000), direction(Loki `forward`·`backward`), force | ResultSet. aws 접속은 code 2 |
| `health` | conn(모든 종류) | `{ok, latency_ms, version, metrics:[{name,value,warn}], summary, err}`(R64.2) |
| `flows` | names(비면 모두), prod | `[{flow, boxes:[{stage, name, metrics, warn, idle, err, note, conn}], opens?:[{path?, sql?}]}]`(R69). opens[i] 는 boxes[i] 상세를 여는 길이다: path 는 `source_open` 길, sql 은 아웃박스·종류 줄 상세 읽기 문장(`run` 으로 보냄). 화면이 원격에서도 flows.json 없이 연다 |
| `outbox_exhausted` | | `[{flow, conn, exhausted}]`(R64 카드). 재시도 열이 있는 아웃박스만, 세지 못하면 -1. AWS 를 부르지 않는다 |
| `board_list` | | `[{name, title, panels, error?}]` |
| `board_run` | name, prod | `{title, error?, every_secs?, panels:[{title, conn, flow, view, prod, value?, warn, result?, boxes?, error?, note?, query?, since?}]}`(R70). every_secs 는 refresh 간격(없으면 60초), query·since 는 Enter 로 쿼리 탭에 열 패널 질의다 |
| `decrypt` | conn(모든 종류), cells | `{values:[글\|null], failed}`(R37). 암호문 꼴(`<32 hex>^\|^<hex>`)이 아니거나 풀지 못한 칸은 null 이고 failed 는 꼴인데 못 푼 수다. 그룹이 없거나 `.env`에 키가 없으면 code 2 |
| `export_start` | conn, sql | `{export, note?}`(R33.1, 아래 「전체 내보내기」) |
| `export_read` | export | `{data, done, bytes, rows}`. data 는 CSV 바이트의 base64 |
| `export_cancel` | export | `{canceled}` |
| `conn_get` | name | `{name, group, kind, host, port, db, user, url, auth, region, profile, workgroup, output, sslmode, has_password}`(db 전용 접속만, 비밀번호 없음) |
| `conn_test` | name 또는 conn(conn_get 칸 + password), old_name | `{version, warning}`(R59.2). 실패는 code 1 `접속 실패: …` |
| `conn_save` | old_name(비면 더하기), conn | `{conn:ConnSummary, warning, env_error}`(R59.2·R59.3) |
| `conn_delete` | name | `{}` |
| `conn_folder` | names, folder(비면 그룹으로) | `{}`(R66.2) |
| `read_query` | conn, sql(문장 하나), limit(0 이면 1,000) | ResultSet. `db mcp` 의 query 다. 버리는 연결의 READ ONLY 트랜잭션(30초)에서 돌고, 쓰기·여러 문장·부수 효과 함수는 접속 전에 code 3 `read_only` 다 |

저장소 op 의 prod 는 화면처럼 읽기를 막지 않는다. `flows`·`board_run` 은 `prod:false` 면 prod 접속을 묻지 않고 그 자리에 note `--prod 로 확인`을 둔다. 저장소 접속은 serve 안에서 접속마다 한 번 열어 다시 쓴다.

ResultSet 은 `{cols, types?, rows, text, truncated, elapsed_ms, in_tx?, note?, origin?}`다. NULL 칸은 `"NULL"`, 64KiB 넘는 칸은 `…(N바이트)`로 잘린다. origin 은 편집기 결과가 한 테이블에서 왔고 PK 열이 모두 있을 때만 있다(`{table:{schema,table}, pk, cols, always}`, cols[i] 는 결과 i 열의 테이블 열 이름이고 식·생성 열은 빈 글). 같은 열이 두 번 나오거나 시스템 열이 섞이면 없다.

## 복호화 보기

`decrypt`는 화면이 보이는 칸을 보내면 serve 가 그 접속 그룹의 `<그룹>_AES_KEY`로 풀어 돌려준다. 키는 serve 밖으로 나가지 않는다. 화면은 `idb_core::statcrypt::looks_encrypted`로 고른 칸만 보내면 줄이 짧다. 푼 값은 화면에만 두고 기록·내보내기·IN 목록·비교는 원래 값을 쓴다(R37). 오류 글은 상태 줄 글 그대로다(`접속 그룹을 몰라 복호화 키를 고를 수 없음`, `APP_AES_KEY 가 .env 에 없음`).

## 전체 내보내기

`export_start`는 문장을 거른 뒤 COPY 를 serve 안의 따로 도는 작업으로 띄우고 id 를 돌려준다. 거절은 code 2 이고 글은 화면 글이다: 문장이 둘 이상이면 `문장 하나만 전체 내보내기 (커서 문장으로 다시 실행)`, 읽기 문장(R6.1)이 아니면 `읽기 문장만 전체 내보내기`, `select`·`with`·`values`·`table`로 시작하지 않으면 `SELECT 문만 전체 내보내기`, `:이름` 파라미터가 있으면 `파라미터(:name·$n)가 있는 문장은 전체 내보내지 않음`이다. enc()는 serve 가 바꾸고 알림은 note 다(R52).

COPY 는 Go `CopyOut`처럼 편집기 세션이 아니라 풀 연결의 `BEGIN READ ONLY` 트랜잭션에서 `COPY (<문장>\n) TO STDOUT WITH (FORMAT csv, HEADER)`로 돈다. 편집기에서 고른 데이터베이스(R27.1)와 `search_path`(R27.2, `SET LOCAL`)를 따르고, 편집기의 열린 트랜잭션을 기다리거나 막지 않는다. 읽기만 하는 트랜잭션이라 prod 도 묻지 않고 잠금(R54)에 걸리지 않는다. 첫 단어 판정이 놓친 CTE 안 쓰기는 서버의 READ ONLY 가 거절한다.

클라이언트는 `done`이 올 때까지 `export_read`를 부른다. 조각은 1MiB 안팎이고 serve 는 조각 4개까지만 쌓는다. 클라이언트가 읽지 않으면 COPY 가 멈추고, 5분 넘게 읽지 않으면 버린 것으로 보고 연결을 닫는다. data 는 CSV 바이트를 글자 경계와 상관없이 자른 것이라 base64 다. bytes 는 지금까지 받은 합, done 일 때 rows 는 COPY 행 수다. done·오류 뒤 그 id 는 사라진다. 파일은 클라이언트가 쓴다(`~/Downloads`, 없으면 임시 폴더, 새 파일만, R14). serve 쪽에 파일을 만들지 않아 원격도 같다.

멈추려면 `export_cancel`을 보낸다. 그 id 의 작업을 멈추고 COPY 연결은 풀에 돌려주지 않고 닫는다. 기다리던 `export_read`를 `cancel`로 끊는 것은 그 기다림만 끊고 내보내기는 이어지며 조각도 잃지 않는다. 접속을 고치거나 지우면(아래) 그 접속의 내보내기도 멈춘다.

## 접속 편집

`conn_get`·`conn_save`·`conn_delete`와 비밀번호가 실린 `conn_test`는 화면이 그 기계의 프로젝트를 쓸 때(`Engine::Local`)만 받는다. `db serve`는 code 3 `remote_conn_edit`(`원격 serve 에서는 접속을 고치지 않음 (그 기계에서 db conn 으로 고침)`)로 거절하고 확인으로도 풀리지 않는다. 까닭은 둘이다. 이 프로토콜은 어느 줄에도 비밀번호를 싣지 않는다는 약속을 지켜야 하고, 원격 프로젝트는 그 기계의 CLI·다른 클라이언트가 같이 쓰는 원본이라 Mac 화면에서 고칠 곳이 아니다. 원격 접속은 그 기계에서 `db conn add|edit|rm`(R59.4)으로 고친다. 저장된 접속을 이름으로 시험하는 `conn_test {name}`와 비밀값이 없는 `conn_folder`만 원격에서도 받는다. 창의 값으로 시험하는 `conn_test {conn}`은 비밀번호 칸이 비어도 원격에서 거절한다. 빈 비밀번호와 저장된 이름, 고른 호스트를 함께 보내 저장된 비밀번호를 다른 호스트로 흘릴 수 있어서다.

`conn_save`는 `db conn add|edit`와 같은 파일 쓰기다(`.dbtui/connections.json`을 쓰고 `.env`에 `PG_<이름>_PASSWORD` 등을 URL 인코딩해 둔다, R59.1). 시험은 하지 않는다. 화면은 `conn_test`를 먼저 부르고 실패하면 `저장할까요? y/N`을 묻는다(R59.2). 이름은 영숫자·`-`·`_`만, DataGrip 접속은 `DataGrip 접속은 DataGrip 에서 고침`으로 code 2 다. 고칠 때 비밀번호가 비면 그대로 두고, 이름이 바뀌면 `.env` 키를 옮긴다. `.env`를 고치지 못하면 접속은 저장된 채 env_error 에 `connection saved but .env was not updated: …`가 온다. URL 붙여 넣기는 화면이 `idb_core::connstore::parse_conn_url`·`parse_http_url`로 칸을 채운다. 평문·AWS 밖 서명 경고는 warning 이다.

저장·삭제·폴더 옮기기 뒤 엔진은 접속 파일을 다시 읽어 `project`가 바로 바뀐다. 고치거나 지운 접속의 세션·풀·Redis·저장소 연결과 내보내기는 닫는다(R59.3, 열린 트랜잭션은 서버가 되돌린다). 파일의 DB 를 고쳤으면 `switch_db`로 고른 데이터베이스를 풀고, 그대로면 지킨다. 폴더만 옮기면 연결을 닫지 않는다(R66.3). 오류 글에서는 받은 비밀번호와 프로젝트의 모든 비밀번호·AES 키를 지운다.

## 결과 표 편집

ChangeSet 은 `{table:{schema,table}, pk:[…], changes:[{op:"update"|"insert"|"delete", key:{pk열:값}, values:{열:값|null}, old:{열:값|null}}], override?}`다. 화면은 결과의 origin 으로 이것을 만든다. serve 는 pk 가 그 테이블의 진짜 PK 와 같은지 확인하고(아니면 code 2) 편집기 세션에서 한 트랜잭션(열린 트랜잭션이면 SAVEPOINT)으로 바꿈마다 하나씩 보낸다. UPDATE·DELETE 가 정확히 한 행이 아니거나 old 가 지금 값과 다르면 묶음 전체를 되돌린다. 답의 applied(`{rows, generated, no_returning}`)를 그대로 `undo_changes`에 넘기면 되돌린다. RETURNING 권한이 없으면 no_returning 이 켜지고 되돌릴 수 없다.

## Redis 연결

편집기 명령(`redis_run`·`redis_edit`)은 세션 연결이고 값 보기·트리·모니터·일괄 삭제는 따로 연결이다. 세션 연결이 끊기면 그 명령은 실행 오류이고, 다음 세션 요청 한 번은 실행하지 않고 `이전 Redis 세션 연결이 끊겨 SELECT 한 DB 같은 세션 상태가 사라졌다…`를 알린 뒤 그다음부터 새 연결로 돈다. 취소로 끊은 것은 알리지 않는다.

편집기 길(`run`·`explain`·`table_page`·`tx_*`)은 접속마다 연결 하나를 계속 쓰는 세션이라 `BEGIN`·`SET`이 다음 요청까지 이어진다. prod 의 읽기 전용이 아닌 문장은 잠겨 있으면 `prod_locked`, 열려 있어도 `confirm_prod`가 없으면 거절이다. `commit`·`rollback`·`end`·`abort`만의 문장은 잠금에 막히지 않는다(R54.2). 잠긴 prod 의 읽기는 READ ONLY 트랜잭션(열린 트랜잭션이면 SAVEPOINT)으로 감싼다. `prod_lock` 상태는 serve 프로세스 안에만 있고, 열리는 시간은 serve 쪽 `DB_PROD_UNLOCK_MIN`이다.

`cli`는 `db <명령> --remote`가 쓴다. 받는 명령은 conns·schemas·tables·describe·ddl·query·explain·search·redis·linked·schema-diff·user360·logs·metrics·status·flows·board 이고 출력 글은 serve 쪽 CLI 가 만든 그대로다. 클라이언트의 `-p`와 `-f`는 쓰지 않는다. `-f` 파일과 표준 입력은 클라이언트가 읽어 `input`으로 보낸다(query·explain·redis·logs·metrics). 단, `flows -f`는 흐름 이름이라 그대로 넘긴다. serve 쪽 파일을 읽게 하지 않으려는 것이다.

## 원격 설정

`db <명령> --remote <이름>`은 `$XDG_CONFIG_HOME/db/remote.json`(없으면 `~/.config/db/remote.json`)에서 이름을 찾는다.

```json
{"devbox": {"ssh": "devbox", "db": "~/.local/bin/db", "project": "~/work/db"}}
```

띄우는 명령은 `ssh -o BatchMode=yes -o ConnectTimeout=15 -T devbox '~/.local/bin/db serve -p ~/work/db'`다. 시스템 ssh 라 `~/.ssh/config`·키·ProxyJump 가 그대로 듣고, 비밀번호·호스트 키를 묻지 않고 실패한다. ssh 가 끝나면 종료 코드와 ssh 가 남긴 마지막 글을 오류에 붙인다.

`DB_REMOTE_CMD`는 시험 전용이다. 있으면 설정 파일과 ssh 대신 그 명령(공백으로 나눔, 예: `db-rs serve -p /tmp/proj`)을 띄운다.
