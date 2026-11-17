# Rust 판으로 넘기기

Rust 판은 `rs-v*` 태그로 미리 보기(prerelease) 릴리스를 낸다(`.github/workflows/release-rust.yml`). Go 판 `v*` 릴리스(`release.yml`)와 이름이 겹치지 않고, prerelease 라 `/releases/latest`·`db update`·설치 한 줄이 집어 가지 않는다. 넘기는 날 이 문서 순서대로 Go 를 지우고 Rust 판을 `v1.0.0` 으로 낸다.

정해진 것: 묶음 이름(`db_<os>_<arch>.tar.gz`, 안에 `db` 하나)과 `checksums.txt` 꼴(`sha256sum ./*` 출력)은 Go 판과 같다. 그래서 Go 판 `db update` 도 v1.0.0 을 받아 Rust 판으로 바뀐다. GUI 는 `i-db_darwin_arm64.zip`(ad-hoc 서명, 공증 없음)이고 같은 `checksums.txt` 에 줄이 있다.

불확실한 것: macOS amd64 GUI 는 내지 않는다(GPUI 를 한 번 더 빌드하는 값에 비해 쓰는 기계가 없다). `ubuntu-24.04-arm` 러너에 `gh` 가 없으면 apt 로 받게 해 두었지만 실제 러너에서 돌려 본 적은 아직 없다.

## 넘기는 날

1. 갈래를 모은다. `feat/rust-tui`(TUI·GUI 포함)와 이 갈래를 `main` 에 합치고 CI 가 초록인지, 골든 시험이 모두 Rust 판으로 도는지(`testdata/golden/rust-ported.txt`) 본다. CI 는 리눅스라 macOS 에서만 빌드되는 GUI 화면 코드를 보지 않는다. 합친 뒤 Mac 에서 `cargo build -p idb-gui` 를 한 번 돌린다(2026-11-17 `feat/rust-tui` 는 `RunRequest.confirm_analyze` 가 빠져 macOS 빌드가 깨져 있었다).

2. 바이너리 이름을 `db` 로 바꾼다. 스위치는 `crates/idb-cli/Cargo.toml` 의 `[[bin]] name` 하나다. 릴리스 작업은 이름을 `cargo metadata` 로 읽고, `update_cmd.rs` 의 `RELEASE_BIN` 은 이미 `db` 다. 나머지는 시험 매크로와 글이다.

   ```sh
   sed -i 's/^name = "db-rs"$/name = "db"/' crates/idb-cli/Cargo.toml
   grep -rl 'CARGO_BIN_EXE_db-rs' crates | xargs sed -i 's/CARGO_BIN_EXE_db-rs/CARGO_BIN_EXE_db/g'
   grep -rn 'db-rs' crates docs .github   # 남은 것: main.rs 안내 글, local.rs hello 의 server, remote.rs·serve-protocol.md 예시, Cargo.toml·update_cmd.rs 주석
   ```

   `main.rs` 의 "only CLI commands are ported yet" 안내는 TUI 가 들어오면 TUI 실행으로 바뀐다. hello 의 `server` 는 판 확인에 쓰지 않아 `db` 로 바꿔도 옛 클라이언트가 깨지지 않는다.

3. Go 코드를 지운다.

   ```sh
   git rm -q *.go go.mod go.sum
   git rm -rq internal
   ```

   `testdata/golden/out/*` 는 Go 판이 만든 기대 출력이라 남긴다. 지운 뒤로는 `-update-golden` 으로 다시 만들 수 없다. 새 골든 케이스는 Rust 출력을 사람이 확인하고 넣는 것으로 바꾸고 `golden.rs` 머리 주석과 `cases.json` 안내를 그에 맞춰 고친다.

4. 워크플로를 바꾼다.

   - `ci.yml`: Go `test` 작업을 지운다.
   - `release.yml`(Go)을 지우고 `release-rust.yml` 을 `release.yml` 로 옮긴다. `name: release`, 태그 `["v*"]`, concurrency 이름을 바꾸고, 초안 만들기와 `publish` 의 `--prerelease --latest=false` 를 지워 정식·최신 릴리스로 낸다. 머리 주석의 prerelease 설명도 지운다.

5. 문서를 고친다.

   - README: `### go install` 절을 지우고 소스 빌드는 `cargo build --release`(실행 파일 `target/release/db`)로 쓴다. 맨 끝 개발 절의 `go vet`·`go test` 를 `cargo fmt --check`·`cargo clippy --all-targets -- -D warnings`·`cargo test` 로 바꾸고, 릴리스 절에 GUI zip 과 Homebrew cask 를 더한다.
   - SPEC: 맨 끝 기술 줄(`Go, Bubble Tea v1, pgx v5, go-redis v9 …`)을 Rust 판 구성으로 바꾸고, 규칙이 가리키는 Go 식별자(`internal/db.DangerOf`·`ErrNoMemoryUsage`·`HiddenWriteOf` 등)를 Rust 경로로 바꾼다. `grep -n 'internal/' SPEC.md` 로 찾는다.
   - 이 문서의 「미리 보기 릴리스」 절을 지운다.

6. `v1.0.0` 을 낸다.

   ```sh
   git tag v1.0.0 && git push origin v1.0.0
   ```

   릴리스가 공개되면 `checksums.txt` 를 받아 묶음 다섯 개와 줄 다섯 개가 맞는지 본다. Go 판이 깔린 기계는 `db update` 한 번으로 바뀐다(v0.x → v1.0.0).

7. Homebrew cask 를 올린다. `packaging/homebrew/i-db.rb` 를 `homebrew-tap` 의 `Casks/i-db.rb` 로 옮기고 `version "1.0.0"`, `sha256` 은 `checksums.txt` 의 `i-db_darwin_arm64.zip` 줄 값으로 넣는다. 탭 README 표에 한 줄을 더하고 `brew install --cask jeong-jae-hun/tap/i-db` 로 깔리는지 본다. 이후 판은 version·sha256 두 줄만 바꾼다.

되돌리기: v1.0.0 에 문제가 있으면 `db update --version v0.<마지막 Go 판> --force` 로 Go 판으로 돌아간다. 묶음 꼴이 같아 Rust 판 `db update` 도 Go 묶음을 받는다.

## 기계 준비

WSL(devbox)·laptop·desktop 에 Rust 판 `db` 를 깐다. devbox 은 원격 serve 를 하는 쪽이라 Go 판이 아닌 Rust 판이어야 한다(Go 판에는 `serve` 가 없다). 스크립트로 둘 만큼 단계가 많지 않아 명령만 적는다.

```sh
tag=rs-v0.1.0   # 넘긴 뒤에는 v1.0.0, 또는 releases/latest/download 주소
os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m); case $arch in x86_64) arch=amd64 ;; aarch64) arch=arm64 ;; esac
base=https://github.com/Jeong-Jae-Hun/i-db/releases/download/$tag
cd "$(mktemp -d)"
curl -fsSLO "$base/db_${os}_${arch}.tar.gz"
curl -fsSLO "$base/checksums.txt"
grep "db_${os}_${arch}.tar.gz" checksums.txt | shasum -a 256 -c -
tar -xzf "db_${os}_${arch}.tar.gz" db
mkdir -p ~/.local/bin && install -m 755 db ~/.local/bin/db
db help | head -3
```

Mac 두 대는 devbox 의 접속을 쓰는 원격 설정을 둔다. 이미 파일이 있으면 덮지 말고 `devbox` 항목만 더한다.

```sh
mkdir -p ~/.config/db
test -e ~/.config/db/remote.json || cat > ~/.config/db/remote.json <<'EOF'
{"devbox":{"ssh":"devbox","db":"~/.local/bin/db","project":"~/work/db"}}
EOF
db conns --remote devbox   # ssh devbox 이 비밀번호 없이 되어야 한다(BatchMode)
```

zsh 별칭은 dotfiles 의 zsh 설정에 둔다.

```sh
alias dbw='db --remote devbox'
```

MCP 는 지금처럼 `claude mcp add db -- db mcp` 이고, Mac 에서 devbox 접속을 쓰려면 `claude mcp add dbw -- db mcp --remote devbox` 이다. prod 허용(`--allow-prod`)·행 수(`--max-rows`)는 Mac 쪽 MCP 가 정하고, 비밀번호와 요금 막음(`DB_ALLOW_BILLED`)은 devbox 쪽 serve 환경이 정한다.

GUI 는 넘기기 전에는 zip 을 손으로 깐다. 공증하지 않은 앱이라 격리 속성을 지워야 열린다.

```sh
curl -fsSLO "$base/i-db_darwin_arm64.zip"
grep i-db_darwin_arm64.zip checksums.txt | shasum -a 256 -c -
ditto -x -k i-db_darwin_arm64.zip /Applications
xattr -dr com.apple.quarantine /Applications/i-db.app
```

넘긴 뒤에는 `brew install --cask jeong-jae-hun/tap/i-db` 이다.
