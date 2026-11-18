<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/lockup-dark.svg">
    <img src="assets/brand/lockup.svg" alt="i-db" height="48">
  </picture>
</h1>

DataGrip 프로젝트를 그대로 여는 DB 도구다. 터미널 화면(TUI), CLI, macOS 앱이 같은 접속 목록을 쓴다.

`.idea/dataSources.xml`을 접속 목록으로 읽고 비밀번호는 옆의 `.env`에서 가져온다. 그래서 접속을 두 군데서 관리할 일이 없다. PostgreSQL과 Redis를 주로 다루고 OpenSearch·Loki·Prometheus·Grafana·DynamoDB·Athena·AWS 리소스는 읽기 전용으로 같은 트리에 붙는다. 동작은 규칙 단위로 [SPEC.md](SPEC.md)에 적혀 있다.

## 설치

릴리스마다 linux·darwin(amd64·arm64) 바이너리와 `checksums.txt`가 올라간다.

```sh
os=$(uname -s | tr A-Z a-z); arch=$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')
curl -fsSL "https://github.com/Jeong-Jae-Hun/i-db/releases/latest/download/db_${os}_${arch}.tar.gz" | tar -xz -C ~/.local/bin db
db update          # 다음부터는 이걸로 올린다
```

macOS 앱은 Homebrew로 깐다.

```sh
brew install --cask jeong-jae-hun/tap/i-db
```

소스에서는 `cargo build --release`로 `db`를, `cargo build --release -p idb-gui --bin i-db`로 앱을 만든다(앱은 Xcode가 필요하다).

## 쓰기

DataGrip `.idea`가 든 폴더에서 `db`를 실행하면 된다. 다른 폴더라면 `db -p <폴더>`나 `DBTUI_PROJECT`로 가리킨다.

```sh
PG_APP_STG_PASSWORD=p%40ssword        # .env, 접속 이름 app-stg
db                                     # TUI
db query -c app-stg "select id from orders limit 3" --format json
db describe -c app-stg orders
db redis -c app-stg-redis GET some:key
db mcp                                 # AI 에이전트용 읽기 전용 MCP 서버
```

명령 목록은 `db help`로 본다. TUI에서는 `Ctrl+P` 명령 팔레트로 모든 동작을 찾을 수 있다.

## 원격 실행

접속 정의와 VPN이 다른 기계(예: WSL)에만 있으면 `db --remote <이름>`을 쓴다. 이렇게 하면 `ssh <호스트> db serve`로 그 기계의 엔진을 빌려 쓴다. CLI, TUI, MCP, 앱 모두 된다. 대상은 `~/.config/db/remote.json`에 적는다.

```json
{"devbox": {"ssh": "devbox", "db": "~/.local/bin/db", "project": "~/work/db"}}
```

프로토콜은 [docs/serve-protocol.md](docs/serve-protocol.md)에 있다.

## 안전장치

- 이름에 `prod`·`prd`가 들어가거나 호스트를 `.`·`-`로 나눈 마디 하나가 `prod`·`prd`인 접속은 prod로 본다. prod는 세션마다 쓰기가 잠겨 있다. `Ctrl+L`(⌘L)로 잠깐 열어도 쓰기마다 다시 묻는다.
- WHERE 없는 DELETE·UPDATE, TRUNCATE, DROP, FLUSHALL은 어디서든 먼저 묻는다.
- CLI는 prod에 쓰지 않는다. MCP는 읽기만 한다.
- 요청마다 요금이 붙는 AWS 조회(Athena, DynamoDB 데이터, SQS·SNS·S3)는 `DB_ALLOW_BILLED=1` 없이는 보내지 않는다.
- 비밀번호와 AES 키는 상태 줄·기록·로그·오류 글에 나오지 않는다.

## 개발

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

`v*` 태그를 올리면 `.github/workflows/release.yml`이 테스트를 거쳐 바이너리와 앱을 만든다. CLI 출력은 `testdata/golden`이 고정한다. 브랜드 규칙은 [assets/brand/README.md](assets/brand/README.md)에 있다.

라이선스는 아직 정하지 않았다. 라이선스 파일이 생기기 전까지는 어떤 권리도 주지 않는다.
