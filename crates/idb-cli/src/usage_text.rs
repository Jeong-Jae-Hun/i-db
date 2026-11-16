//! Go 판 conncli.go·historycli.go·update.go 의 사용법 글을 그대로 옮겼다. 골든 비교가 글자 하나까지 본다.

pub const CONN_USAGE: &str = r#"사용법:
  db conn add <이름> --url URL [--group 그룹] [--kind 종류] [--user 사용자] [--auth A] [--region R] [--profile P] [--workgroup W] [--output S3] [--password-stdin] [--force]
  db conn edit <이름> [--url URL] [--host 호스트] [--port 포트] [--db DB] [--user 사용자] [--group 그룹] [--auth A] [--region R] [--profile P] [--workgroup W] [--output S3] [--password-stdin] [--force]
  db conn rm <이름> [--yes]
  db conn test <이름> | --url URL
  db conn folder <이름> <폴더> | <이름> --reset | --rename <옛 폴더> <새 폴더>

  접속은 .dbtui/connections.json, 비밀번호는 .env 의 PG_<이름>_PASSWORD·REDIS_<이름>_PASSWORD(URL 인코딩)에 둔다.
  읽기 전용 저장소(R60)는 --kind opensearch|loki|prometheus|grafana|dynamodb|athena 와 http(s) --url 로 더한다(DynamoDB 의 --url 은 선택 엔드포인트, Athena 는 --url 없음).
  Athena(R68)는 --region 이 필요하고 --workgroup(비우면 primary)·--output s3://bucket/prefix/(비우면 작업 그룹 설정)를 받는다.
  --auth 는 basic·sigv4(OpenSearch)·token(Grafana), sigv4·DynamoDB 는 --region 이 필요하다. 비밀값 키는 OS_·LOKI_·PROM_·GRAFANA_<이름>_TOKEN 이다.
  비밀번호는 --url 또는 --password-stdin 으로만 받고 찍지 않는다. edit 에서 주지 않으면 그대로 둔다.
  add·edit 는 저장 전에 접속을 시험하고, 실패하면 --force 없이는 저장하지 않는다(종료 코드 1).
  rm 은 묻고 지운다. 표준 입력이 터미널이 아니면 --yes 가 필요하다. DataGrip 접속은 DataGrip 에서 고친다.
  prod 접속(R6.2)에 붙는 add·edit·test 는 --prod 가 필요하다.
  folder 는 트리에 보일 폴더만 바꾼다(.dbtui/folders.json, DataGrip 접속도). 그룹·암호화 키는 그대로다.

공통 플래그: -p 경로 --timeout 30s
종료 코드: 0 성공 · 1 실행 오류(시험 실패 포함) · 2 사용법 · 3 안전장치 거절
"#;

pub const HISTORY_USAGE: &str = r#"사용법:
  db history [--conn 접속] [--since 24h] [--limit 50] [--format table|tsv|json]
  db history show <id> [--format table|tsv|json]

  실행 기록(최근 것부터)을 찍는다. --since 는 지금부터 거슬러 볼 기간(1h·24h·7d)이다.
  result 열의 ◆ 는 결과 사본이 있다는 뜻이고 show <id> 가 그 사본을 찍는다(첫 1,000행·1MiB).
  비밀값이 든 문장은 기록 때 이미 가렸고 사본도 없다.

종료 코드: 0 성공 · 1 실행 오류(사본 없음 포함) · 2 사용법
"#;

pub const UPDATE_USAGE: &str = r#"사용법:
  db update [--check] [--version vX.Y.Z] [--force] [--timeout 2m]

  GitHub 릴리스(Jeong-Jae-Hun/i-db)의 최신 판(또는 --version 판)을 받아 checksums.txt 로 검증하고
  지금 실행 중인 db 를 바꿔 끼운다. 심볼릭 링크면 가리키는 파일을 바꾼다. GITHUB_TOKEN 이 있으면 API 요청에 붙인다.
  --check 는 바꾸지 않고 새 판 여부만 보인다. 낮은 판으로 내리거나 소스 빌드(dev)를 바꿀 때는 --force 가 필요하다.

종료 코드: 0 성공·이미 최신 · 1 실행 오류 · 2 사용법 · 3 --force 가 필요함 · 10 새 판 있음(--check)
"#;
