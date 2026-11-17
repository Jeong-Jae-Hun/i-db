# db serve 프로토콜

`db serve -p <프로젝트>`는 표준 입출력으로 엔진 요청을 받는다. 포트를 열지 않는다. Mac 의 `db`는 `ssh <호스트> <db> serve -p <프로젝트>`를 띄워 WSL 에 있는 접속을 쓴다. 접속 정의·`.env` 비밀번호·VPN 경로는 serve 쪽에만 있고, 안전장치(읽기 전용, prod 확인·잠금, 위험 문장, 가리기)도 serve 쪽이 정한다. 클라이언트는 사용자가 확인했다는 표시만 보낸다.

## 줄

한 줄에 JSON 하나다. 요청은 `{"id":N,"op":"…",…}`이고 응답은 `{"id":N,"ok":true,"result":…}` 또는 `{"id":N,"ok":false,"code":3,"error":"…","reason":"…"}`이다. id 는 클라이언트가 고르는 양의 정수이고 진행 중인 요청끼리 겹치면 안 된다. 요청은 동시에 돌고 응답은 끝난 순서로 온다. 모르는 필드는 무시한다.

code 는 CLI 종료 코드와 같다. 1 실행 오류, 2 요청·사용법 오류, 3 안전장치 거절이다. code 3 의 reason 은 `prod_locked`(R54, 확인 창 없이 거절), `confirm_prod`(R6, 확인 후 `confirm_prod:true`로 다시 보냄), `confirm_danger`(R25, `confirm_danger:true`), `confirm`(백엔드 신호·패턴 일괄 삭제처럼 환경과 상관없이 늘 묻는 것, `confirm:true`), `confirm_analyze`(읽기 전용이 아닌 문장의 EXPLAIN ANALYZE, R9.2, `confirm_analyze:true`)이다. Redis `KEYS`는 접속하기 전에 `keys`로 거절한다(확인으로 풀리지 않음, SCAN 을 쓴다). 저장소 op 는 `read_only`(쓰기로 보이는 질의, 다시 보낼 길 없음), `confirm_expensive`(전체 스캔·7일 넘는 범위 같은 비싼 질의 R60.2, 확인 후 `force:true`), `billed`(요청마다 요금이 붙는 AWS 조회 R72, serve 쪽 `DB_ALLOW_BILLED=1` 없이는 확인으로도 풀리지 않음)를 쓴다. error 글은 그대로 확인 창에 보일 수 있는 질문(`prod(app-prod)에 제출 2건: y/N`)이다. 편집기 실행이 실패했는데 트랜잭션이 열려 있으면 code 1 에 reason `in_tx`가 붙는다.

어느 메시지에도 비밀번호·AES 키를 싣지 않는다. 오류 글은 serve 가 그 접속의 비밀번호와 모든 AES 키·IV 를 지우고 제어 문자를 바꾼 것이다. 단, staticEncrypt 암호문은 형식상 앞에 IV 를 싣는다(`IV^|^…`).

## 인사와 취소

첫 요청은 `{"id":1,"op":"hello","version":1}`이어야 한다. 답은 `{"version":1,"server":"db-rs"}`다. 그 전 요청과 다른 판은 code 2 다.

`{"id":9,"op":"cancel","target":5}`는 진행 중인 5 를 멈춘다. 5 는 code 1 `canceled`로 끝나고 9 의 답은 `{"canceled":true}`다(이미 끝났으면 false). PostgreSQL 실행을 멈추면 세션 연결이 닫히고 다음 실행은 한 번 R3.6 세션 유실 오류를 낸다. 단, `cli`는 serve 안에서 막는 작업이라 응답만 끊기고 명령은 `--timeout`까지 돈다.

판 1 은 op 를 더하기만 한다. 옛 클라이언트는 새 op 를 쓰지 않으면 그대로 돌고, 새 클라이언트가 옛 serve 에 새 op 를 보내면 code 2 `bad request: unknown variant …`다.

입력이 닫히면 serve 는 남은 요청을 60초까지 기다려 답한 뒤 멈추고, 세션을 닫아 열린 트랜잭션을 서버가 되돌리게 한 뒤 끝난다.

## op

| op | 필드 | result |
| --- | --- | --- |
| `project` | | `{dir, conns:[{name,group,folder,kind,env,host,db,source}], warnings}` |
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
| `source_open` | conn, path | ResultSet(트리 Enter 미리 보기, Loki 100줄·나머지 50행) |
| `source_run` | conn, text, since_ns, until_ns, limit(기본 1,000), direction(Loki `forward`·`backward`), force | ResultSet. aws 접속은 code 2 |
| `health` | conn(모든 종류) | `{ok, latency_ms, version, metrics:[{name,value,warn}], summary, err}`(R64.2) |
| `flows` | names(비면 모두), prod | `[{flow, boxes:[{stage, name, metrics, warn, idle, err, note, conn}]}]`(R69) |
| `board_list` | | `[{name, title, panels, error?}]` |
| `board_run` | name, prod | `{title, error?, panels:[{title, conn, flow, view, prod, value?, warn, result?, boxes?, error?, note?}]}`(R70) |

저장소 op 의 prod 는 화면처럼 읽기를 막지 않는다. `flows`·`board_run` 은 `prod:false` 면 prod 접속을 묻지 않고 그 자리에 note `--prod 로 확인`을 둔다. 저장소 접속은 serve 안에서 접속마다 한 번 열어 다시 쓴다.

ResultSet 은 `{cols, types?, rows, text, truncated, elapsed_ms, in_tx?, note?, origin?}`다. NULL 칸은 `"NULL"`, 64KiB 넘는 칸은 `…(N바이트)`로 잘린다. origin 은 편집기 결과가 한 테이블에서 왔고 PK 열이 모두 있을 때만 있다(`{table:{schema,table}, pk, cols, always}`, cols[i] 는 결과 i 열의 테이블 열 이름이고 식·생성 열은 빈 글). 같은 열이 두 번 나오거나 시스템 열이 섞이면 없다.

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
