# db serve 프로토콜

`db serve -p <프로젝트>`는 표준 입출력으로 엔진 요청을 받는다. 포트를 열지 않는다. Mac 의 `db`는 `ssh <호스트> <db> serve -p <프로젝트>`를 띄워 WSL 에 있는 접속을 쓴다. 접속 정의·`.env` 비밀번호·VPN 경로는 serve 쪽에만 있고, 안전장치(읽기 전용, prod 확인·잠금, 위험 문장, 가리기)도 serve 쪽이 정한다. 클라이언트는 사용자가 확인했다는 표시만 보낸다.

## 줄

한 줄에 JSON 하나다. 요청은 `{"id":N,"op":"…",…}`이고 응답은 `{"id":N,"ok":true,"result":…}` 또는 `{"id":N,"ok":false,"code":3,"error":"…","reason":"…"}`이다. id 는 클라이언트가 고르는 양의 정수이고 진행 중인 요청끼리 겹치면 안 된다. 요청은 동시에 돌고 응답은 끝난 순서로 온다. 모르는 필드는 무시한다.

code 는 CLI 종료 코드와 같다. 1 실행 오류, 2 요청·사용법 오류, 3 안전장치 거절이다. code 3 의 reason 은 `prod_locked`(R54, 확인 창 없이 거절), `confirm_prod`(R6, 확인 후 `confirm_prod:true`로 다시 보냄), `confirm_danger`(R25, `confirm_danger:true`)다. 편집기 실행이 실패했는데 트랜잭션이 열려 있으면 code 1 에 reason `in_tx`가 붙는다.

어느 메시지에도 비밀번호·AES 키를 싣지 않는다. 오류 글은 serve 가 그 접속의 비밀번호와 모든 AES 키·IV 를 지우고 제어 문자를 바꾼 것이다. 단, staticEncrypt 암호문은 형식상 앞에 IV 를 싣는다(`IV^|^…`).

## 인사와 취소

첫 요청은 `{"id":1,"op":"hello","version":1}`이어야 한다. 답은 `{"version":1,"server":"db-rs"}`다. 그 전 요청과 다른 판은 code 2 다.

`{"id":9,"op":"cancel","target":5}`는 진행 중인 5 를 멈춘다. 5 는 code 1 `canceled`로 끝나고 9 의 답은 `{"canceled":true}`다(이미 끝났으면 false). PostgreSQL 실행을 멈추면 세션 연결이 닫히고 다음 실행은 한 번 R3.6 세션 유실 오류를 낸다. 단, `cli`는 serve 안에서 막는 작업이라 응답만 끊기고 명령은 `--timeout`까지 돈다.

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
| `explain` | run 필드 + analyze, buffers, format | ResultSet |
| `table_page` | conn, schema, table, where, order_by, offset, limit(기본 200, 상한 1,000), count, estimate | `{result, sql, count?, estimate?}` |
| `tx_begin`·`tx_rollback`·`tx_state` | conn | `{in_tx, text}` |
| `tx_commit` | conn, confirm_prod | `{in_tx, text}` |
| `prod_lock` | conn, unlock(true 열기·false 잠그기·없으면 보기) | `{conn, prod, locked, remaining_secs, unlock_min}` |
| `redis_scan` | conn, pattern(기본 `*`) | `{keys, more}`(1,000개까지, 정렬) |
| `redis_value` | conn, key | ResultSet(머리 글은 text, 컬렉션은 표) |
| `redis_run` | conn, args, confirm_prod, confirm_danger | ResultSet |
| `cli` | args, input(`{"text":…}`·`{"error":…}`), tty | `{stdout, stderr, code}` |

ResultSet 은 `{cols, types?, rows, text, truncated, elapsed_ms, in_tx?, note?}`다. NULL 칸은 `"NULL"`, 64KiB 넘는 칸은 `…(N바이트)`로 잘린다.

편집기 길(`run`·`explain`·`table_page`·`tx_*`)은 접속마다 연결 하나를 계속 쓰는 세션이라 `BEGIN`·`SET`이 다음 요청까지 이어진다. prod 의 읽기 전용이 아닌 문장은 잠겨 있으면 `prod_locked`, 열려 있어도 `confirm_prod`가 없으면 거절이다. `commit`·`rollback`·`end`·`abort`만의 문장은 잠금에 막히지 않는다(R54.2). 잠긴 prod 의 읽기는 READ ONLY 트랜잭션(열린 트랜잭션이면 SAVEPOINT)으로 감싼다. `prod_lock` 상태는 serve 프로세스 안에만 있고, 열리는 시간은 serve 쪽 `DB_PROD_UNLOCK_MIN`이다.

`cli`는 `db <명령> --remote`가 쓴다. 받는 명령은 conns·schemas·tables·describe·ddl·query·explain·search·redis·linked·schema-diff·user360·logs·metrics·status·flows·board 이고 출력 글은 serve 쪽 CLI 가 만든 그대로다. 클라이언트의 `-p`와 `-f`는 쓰지 않는다. `-f` 파일과 표준 입력은 클라이언트가 읽어 `input`으로 보낸다. serve 쪽 파일을 읽게 하지 않으려는 것이다.

## 원격 설정

`db <명령> --remote <이름>`은 `$XDG_CONFIG_HOME/db/remote.json`(없으면 `~/.config/db/remote.json`)에서 이름을 찾는다.

```json
{"devbox": {"ssh": "devbox", "db": "~/.local/bin/db", "project": "~/work/db"}}
```

띄우는 명령은 `ssh -o BatchMode=yes -o ConnectTimeout=15 -T devbox '~/.local/bin/db serve -p ~/work/db'`다. 시스템 ssh 라 `~/.ssh/config`·키·ProxyJump 가 그대로 듣고, 비밀번호·호스트 키를 묻지 않고 실패한다. ssh 가 끝나면 종료 코드와 ssh 가 남긴 마지막 글을 오류에 붙인다.

`DB_REMOTE_CMD`는 시험 전용이다. 있으면 설정 파일과 ssh 대신 그 명령(공백으로 나눔, 예: `db-rs serve -p /tmp/proj`)을 띄운다.
