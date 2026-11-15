package main

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"sync/atomic"
	"time"

	"github.com/aymanbagabas/go-osc52/v2"
	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/x/ansi"
	"github.com/charmbracelet/x/term"

	"github.com/Jeong-Jae-Hun/i-db/internal/complete"
	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/editor"
	"github.com/Jeong-Jae-Hun/i-db/internal/favorites"
	"github.com/Jeong-Jae-Hun/i-db/internal/finder"
	"github.com/Jeong-Jae-Hun/i-db/internal/grid"
	"github.com/Jeong-Jae-Hun/i-db/internal/history"
	"github.com/Jeong-Jae-Hun/i-db/internal/keys"
	"github.com/Jeong-Jae-Hun/i-db/internal/notify"
	"github.com/Jeong-Jae-Hun/i-db/internal/params"
	"github.com/Jeong-Jae-Hun/i-db/internal/paramstore"
	"github.com/Jeong-Jae-Hun/i-db/internal/plantree"
	"github.com/Jeong-Jae-Hun/i-db/internal/redisview"
	"github.com/Jeong-Jae-Hun/i-db/internal/session"
	"github.com/Jeong-Jae-Hun/i-db/internal/sqlfmt"
	"github.com/Jeong-Jae-Hun/i-db/internal/statcrypt"
	"github.com/Jeong-Jae-Hun/i-db/internal/tableview"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

const (
	minWidth  = 60
	minHeight = 14

	defaultTreeWidth = 34
	minTreeWidth     = 20
	maxTreePercent   = 60 // 트리 폭 상한은 화면 폭의 60%(R46.1)
	treeWidthStep    = 2
	minRightWidth    = 20

	defaultEditorRatio = 35 // 편집기 높이를 정하지 않았을 때(0) 본문에서 차지하는 비율(%)
	minEditorLines     = 3  // 편집기·결과 높이 하한(R46.1)
	editorStep         = 2

	wheelStep   = 3
	doubleClick = 400 * time.Millisecond
	tabPlus     = " + "

	loadTimeout = 30 * time.Second
	historyMax  = 500
	placeholder = "SQL 또는 Redis 명령 · Ctrl+R 실행 · Ctrl+Space 자동완성"
	// 빈 편집기의 거터(4칸)와 커서 칸 뒤에 안내 글을 그린다
	placeholderX = 5

	maxPins = 5 // 탭마다 고정할 수 있는 결과 수

	// Redis 모니터(R34.1): SLOWLOG GET 개수, 큰 키 표본·상위 수
	redisSlowLogN     = 50
	redisBigKeySample = 1000
	redisBigKeyTop    = 50

	// Redis 패턴 일괄 삭제(R42): 한 번에 지울 키 상한, UNLINK 배치, 확인 창 미리 보기 수
	redisDeleteLimit   = 100000
	redisDeleteBatch   = 500
	redisDeletePreview = 20

	estimateTimeout = 3 * time.Second // 쓰기 확인 창의 영향 행 수 추정(R39)을 기다리는 시간
)

const (
	nGroup nodeKind = iota
	nConn
	nSchema
	nTable
	nKey
	nInfo    // "… 더 있음", 오류 등 선택해도 아무것도 안 하는 줄
	nFolder  // Redis 키 이름공간(`user:`)
	nQueries // 쿼리 파일 묶음(R43). 접속 아래 `쿼리 (N)` 이거나 트리 맨 아래 `쿼리 (접속 없음)`
	nFile    // queries/ 파일 하나. key 는 프로젝트 상대 경로다
)

const (
	fTree focus = iota
	fEditor
	fResult

	fKeep focus = -1 // 명령 팔레트가 포커스를 바꾸지 않고 키를 누른다
)

const (
	oNone overlay = iota
	oFilter
	oPicker
	oConfirm
	oHelp
	oHistory
	oParams
	oTx
	oImport
	oFinder
	oPrompt // 한 줄 입력(즐겨찾기 이름·데이터 검색 글). Enter 면 promptDo 를 부른다
	oLong   // 칸 값 전체 화면 편집기(R49.3)
	oConn   // 접속 추가·고치기 창(R59.2)
)

// runKind 는 실행 결과를 받는 방법이다. 표를 바꾸는 종류만 보류 중인 편집을 버릴지 먼저 묻는다.
const (
	rkGrid       runKind = iota // 결과를 표에 붙인다
	rkTable                     // 테이블 보기의 한 페이지
	rkEstimate                  // 테이블 보기의 추정 행 수. 끝나면 페이지를 읽는다
	rkCount                     // 테이블 보기의 정확한 행 수
	rkPlan                      // EXPLAIN (FORMAT JSON) 을 계획 나무로
	rkSubmit                    // 결과 표 편집 제출
	rkTx                        // COMMIT·ROLLBACK
	rkSignal                    // 백엔드 취소·종료
	rkImport                    // CSV 가져오기
	rkRedisWrite                // 값 편집기가 만든 Redis 명령
)

// 확인 창이 묻는 것. 한 실행이 차례로 여러 단계를 거칠 수 있다(편집 버리기 → ANALYZE → prod·제출)
const (
	askNone ask = iota
	askDiscard
	askAnalyze
	askWrite
)

const (
	afterClose txAfter = iota
	afterQuit
	afterActivate
	afterRun
	afterDB // 접속을 다른 데이터베이스로 다시 연다(R27.1)
)

var (
	// 테스트가 OSC 52·OSC 9 출력처를 바꿔 끼운다
	clipboardOut io.Writer = os.Stdout
	// 알림(R30)은 stdout 이 터미널일 때만 보낸다. 테스트가 바꿔 끼운다
	stdoutTTY = func() bool { return term.IsTerminal(os.Stdout.Fd()) }
	// 전체 내보내기 중 받은 바이트를 상태 줄에 다시 그리는 간격
	exportTick = 200 * time.Millisecond
	// 헤더 트랜잭션 시계를 다시 그리는 간격. 테스트가 줄인다
	txTick = time.Second
	// 파일에 이은 탭을 마지막 고침 뒤 자동 저장하기까지 기다리는 시간(R45.2)
	autoSaveDelay = time.Second
	// 키보드로 창 크기를 바꾼 뒤 세션 파일에 쓰기까지 기다리는 시간. 키를 누르고 있어도 한 번만 쓴다(R46.2)
	paneSaveDelay = 500 * time.Millisecond

	// 내보내기 파일 이름에 탭 이름을 넣을 때 경로·셸에 문제 없는 글자만 남긴다
	fileNameRe = regexp.MustCompile(`[^A-Za-z0-9_-]+`)
	// mouseLeakRe 는 ESC 를 잃고 글자로 들어온 SGR 마우스 보고 하나 이상이다
	mouseLeakRe = regexp.MustCompile(`^(\[?<\d+;\d+;\d+[Mm])+$`)

	replacesGrid = map[runKind]bool{rkGrid: true, rkTable: true, rkEstimate: true, rkPlan: true}
	// 계획 나무가 떠 있을 때 나무로 보내는 키. 그 밖의 키(Tab·Ctrl 조합 등)는 평소대로 처리한다
	planKeys = map[string]bool{"esc": true, "q": true, "t": true, "up": true, "k": true, "down": true, "j": true,
		"pgup": true, "pgdown": true, "home": true, "g": true, "end": true, "G": true, "enter": true, " ": true,
		"left": true, "h": true, "right": true, "l": true}
	// 확인 창의 미리 보기를 넘기는 키. 그 밖의 키는 y 가 아니면 취소다
	previewKeys = map[string]int{"up": -1, "k": -1, "down": 1, "j": 1, "pgup": -10, "pgdown": 10}
)

var focusHints = map[focus]string{
	fTree:   "Enter 열기 · d 구조 · D DDL · I CSV · A/L/S 운영 · B DB · F 검색 · X 키 삭제 · N 새 접속 · / 거르기 · ? 도움말",
	fEditor: "Ctrl+R 실행 · Ctrl+E 계획 · Ctrl+B 정의 · F8 정렬 · F9 Auto/Manual · F3/F4 즐겨찾기 · Ctrl+P 팔레트",
	fResult: "F2 고치기 · Ctrl+S 제출 · p 고정 · C IN · F/R 외래 키 · D 복호화 · +/- 거르기 · o 값 · F1 도움말",
}

// helpSections 는 F1 도움말과 명령 팔레트(R40)의 항목이다. 항목은 {키, 설명, 팔레트가 다시 누를 키}이고
// 셋째가 비면 팔레트에 넣지 않는다(마우스·⌘ 조합·설명 줄). at 은 팔레트가 키를 누르기 전에 옮길 포커스다(fKeep 이면 그대로).
var helpSections = []struct {
	title string
	at    focus
	keys  [][3]string
}{
	{"전역", fKeep, [][3]string{
		{"Tab / Shift+Tab", "포커스 이동 (자동완성 창이 열려 있으면 Tab 은 고르기)", "tab"},
		{"Ctrl+R / F5", "실행: 선택한 글 전부, 없으면 커서 문장", "ctrl+r"},
		{"Ctrl+E", "EXPLAIN (실행하지 않음)", "ctrl+e"},
		{"Alt+E", "EXPLAIN ANALYZE, BUFFERS (쓰기 문장은 y/N)", "alt+e"},
		{"Ctrl+P", "명령 팔레트 (동작·테이블을 이름으로 찾아 실행·열기, 최근 테이블 먼저)", ""},
		{"Ctrl+H", "실행 기록", "ctrl+h"},
		{"Ctrl+N", "객체 찾기 (테이블·뷰·Redis 키)", "ctrl+n"},
		{"Ctrl+O / Ctrl+S", "파일 열기 / 저장 (결과 편집이 있으면 제출)", "ctrl+o"},
		{"F9", "트랜잭션 Auto / Manual (Manual 은 실행 전 BEGIN)", "f9"},
		{"F10 / F11", "COMMIT (prod 는 확인) / ROLLBACK", "f10"},
		{"F3 / F4", "즐겨찾기 열기 (새 탭) / 저장 (선택, 없으면 전체)", "f3"},
		{"F6", "스키마 고르기 (search_path)", "f6"},
		{"F7", "마지막 표 결과 전체 내보내기 CSV (esc 취소)", "f7"},
		{"Ctrl+U", "사용자 한눈에 보기 (이메일·uid·id·닉네임으로 여러 DB 조회)", "ctrl+u"},
		{"Alt+C", "같은 쿼리 다른 접속에서 비교 (기본 stg↔prod)", "alt+c"},
		{"Ctrl+L", "prod 쓰기 잠금 열기·닫기 (탭 접속, DB_PROD_UNLOCK_MIN 분)", "ctrl+l"},
		{"Ctrl+C", "실행 취소, 실행 중이 아니면 종료", "ctrl+c"},
		{"Ctrl+← / Ctrl+→", "트리 폭 (트리·결과 포커스, 다음에도 그 크기)", "ctrl+right"},
		{"Ctrl+↑ / Ctrl+↓", "편집기 높이 (트리·결과 포커스, 다음에도 그 크기)", "ctrl+down"},
		{"F1", "도움말", "f1"},
		{"DB_RESTORE=0", "끝낼 때 탭 저장·다음에 되살리기 끄기 (창 크기는 남김)", ""},
		{"DB_AUTO_SAVE·OPEN=0", "자동 저장 / 접속 고를 때 최근 파일 열기 끄기", ""},
		{"DB_KEYS=cmd / ctrl", "키 안내를 ⌘ / Ctrl 로 (기본은 맥·kitty 키면 ⌘)", ""},
		{"enc('글')", "실행 전에 그룹 키·IV 로 staticEncrypt 암호문으로 바꿈", ""},
	}},
	{"맥 ⌘ (Ghostty·kitty·WezTerm·iTerm2)", fKeep, [][3]string{
		{"⌘R / ⌘↩", "실행 (= Ctrl+R)", ""},
		{"⌘E / ⌥⌘E", "EXPLAIN / EXPLAIN ANALYZE", ""},
		{"⌘S / ⌘O", "저장·제출 / 열기", ""},
		{"⌘/ · ⌘D · ⌘K · ⌘A", "주석 · 줄 복제 · 복사 · 전체 선택", ""},
		{"⌘Z / ⌘⇧Z", "되돌리기 / 다시 하기", ""},
		{"⌘B", "정의로 이동 (= Ctrl+B)", ""},
		{"⌘P", "명령 팔레트 (= Ctrl+P)", ""},
		{"⌘T ⌘W ⌘1..9 ⌘N ⌘H ⌘?", "= Ctrl+T Ctrl+W Alt+1..9 Ctrl+N Ctrl+H F1", ""},
		{"⌘⇧O ⌘⇧S ⌘⇧D ⌘⇧E", "= F3 F4 F6 F7", ""},
		{"⌥⌘L ⌘⇧M ⌘⇧↩ ⌘⇧⌫", "= F8(⌘⇧F 도) F9 F10 F11", ""},
		{"⌘U ⌘L ⌘⇧C ⌘⇧R ⌘⇧P", "= Ctrl+U Ctrl+L Alt+C Alt+R Alt+P", ""},
		{"⌘V", "결과 표에 시트 칸 붙여넣기", ""},
		{"⌘←→ / ⌘↑↓", "줄 처음·끝 / 문서 처음·끝", ""},
	}},
	{"쿼리 탭", fKeep, [][3]string{
		{"Ctrl+T", "새 탭", "ctrl+t"},
		{"Ctrl+W", "탭 닫기", "ctrl+w"},
		{"Alt+1..9", "탭 이동", "alt+1"},
		{"Ctrl+PgUp / Ctrl+PgDn", "이전 / 다음 탭 (파일 탭은 옮길 때 자동 저장)", "ctrl+pgdown"},
		{"Alt+R", "탭 이름 바꾸기", "alt+r"},
		{"Alt+P", "탭 고정 (앞에 모으고 닫을 때 묻기)", "alt+p"},
	}},
	{"편집기", fEditor, [][3]string{
		{"Ctrl+Space", "자동완성 (Tab·Enter 고르기, esc 닫기)", "ctrl+@"},
		{"Shift+방향키", "선택", "shift+right"},
		{"Ctrl+← / Ctrl+→", "단어 이동", "ctrl+right"},
		{"Ctrl+↑ / Ctrl+↓", "스크롤", "ctrl+down"},
		{"Ctrl+A", "전체 선택", "ctrl+a"},
		{"Ctrl+X / Ctrl+K / Ctrl+V", "잘라내기 / 복사 / 붙여넣기", "ctrl+k"},
		{"Ctrl+Z / Ctrl+Y", "되돌리기 / 다시", "ctrl+z"},
		{"Ctrl+D", "줄 복제", "ctrl+d"},
		{"Ctrl+/", "주석 토글", "ctrl+_"},
		{"단어 + Tab", "라이브 템플릿 (sel selw selc lim ins upd del cnt)", "tab"},
		{"F8", "SQL 정렬 (선택, 없으면 전체)", "f8"},
		{"Ctrl+B / F12", "정의로 이동 (테이블 구조 · 함수 정의)", "ctrl+b"},
		{":name / $1", "실행 전에 값을 묻고 바인딩", ""},
		{"자동 저장", "파일 없는 탭은 첫 실행 때 queries/ 에 저장, 파일 탭은 고친 1초 뒤 저장", ""},
	}},
	{"트리", fTree, [][3]string{
		{"j k / ↑ ↓", "이동", "j"},
		{"Enter / l", "펼치기 · 테이블·키 열기", "enter"},
		{"h", "접기 · 부모로", "h"},
		{"Enter (테이블)", "테이블 보기 (서버 페이지·거르기·정렬)", "enter"},
		{"Enter (접속)", "펼치기 · 그 접속을 쓰는 탭이 없으면 최근 파일 열기", "enter"},
		{"Enter (쿼리 파일)", "파일 열기 (이미 열린 탭이면 그 탭으로)", "enter"},
		{"d / D", "테이블 구조 / DDL 을 새 탭에", "d"},
		{"I", "CSV 가져오기 (테이블)", "I"},
		{"A / L / S", "활동 / 잠금 / 테이블 크기 (PostgreSQL 접속)", "A"},
		{"A / L / S", "INFO / SLOWLOG / 큰 키 (Redis 접속)", "A"},
		{"B", "데이터베이스 바꾸기 (PostgreSQL 접속)", "B"},
		{"F", "데이터 검색 (PostgreSQL 접속·스키마)", "F"},
		{"X", "패턴 맞는 키 일괄 삭제 (Redis 접속·이름공간, esc 멈춤)", "X"},
		{"N", "새 접속 (URL 붙여넣기 · Ctrl+T 시험, .dbtui/connections.json)", "N"},
		{"E", "접속 고치기 (db 전용 접속)", "E"},
		{"X (PostgreSQL 접속)", "접속 지우기 (db 전용 접속, .env 비밀번호도)", ""},
		{"Alt+X", "접속 지우기 (db 전용 접속, Redis 포함)", "alt+X"},
		{"=", "다른 접속과 스키마 비교 (PostgreSQL 접속·스키마)", "="},
		{"/", "거르기 (Redis 키는 SCAN 패턴)", "/"},
		{"esc", "거르기 해제", "esc"},
		{"r", "새로고침 (접속은 자동완성 목록도, 쿼리 파일 목록도)", "r"},
		{"?", "도움말", "?"},
		{"q", "종료", "q"},
	}},
	{"결과", fResult, [][3]string{
		{"h j k l / 방향키", "셀 이동", "j"},
		{"PgUp / PgDn · g / G", "쪽 이동 · 처음 / 끝", "pgdown"},
		{"0 / $", "첫 열 / 끝 열", "0"},
		{"Enter / v", "셀 전체 값 / 레코드 세로 보기", "enter"},
		{"s · /", "정렬(오름→내림→없음) · 거르기 abc !abc col:abc col=v col>v col=null", "s"},
		{"y / Y", "셀 / 행 복사", "y"},
		{"e / E", "CSV / JSON 내보내기", "e"},
		{"< / >", "열 폭", ">"},
		{"w", "텍스트 줄바꿈 (테이블 보기는 WHERE, / 도 같음)", "w"},
		{"O", "테이블 보기 ORDER BY 입력 (Tab 열 이름 완성)", "O"},
		{"Enter·F2 / esc", "칸 고치기 / 취소 (Ctrl+N 은 NULL)", "f2"},
		{"a · Ctrl+D · d · u", "행 추가 · 복제 · 삭제 표시 · 되돌리기", "a"},
		{"Ctrl+S", "변경 미리 보기 뒤 제출", "ctrl+s"},
		{"Ctrl+Z", "마지막 제출 되돌리기 (미리 보기 뒤 확인)", "ctrl+z"},
		{"붙여넣기", "시트 칸(TSV)을 커서부터 덮어쓰기 (넘치면 새 행)", ""},
		{"칸 고치는 중", "space bool 순환 · now/today · Ctrl+O 큰 편집기 · enum 은 고르기 창", ""},
		{"i / U", "행을 INSERT / UPDATE 문으로 복사", "i"},
		{"o", "셀 전체 값", "o"},
		{"[ / ] · #", "테이블 보기 이전·다음 페이지 · 정확한 개수", "]"},
		{"p / P · { / }", "결과 고정 / 풀기 · 결과 탭 이동", "p"},
		{"K / X", "pid 행의 쿼리 취소 / 연결 종료", "K"},
		{"t · esc", "실행 계획 나무 ↔ JSON · 닫기", "t"},
		{"c", "차트 (다시 c·esc 로 표)", "c"},
		{"V", "사각형 선택 · 합·평균·최소·최대 (y 복사, esc·V 끝)", "V"},
		{"=", "고정한 결과와 비교 (esc 닫기)", "="},
		{"C", "열(선택) 값을 IN 목록으로 복사", "C"},
		{"F / R", "외래 키 따라가기 / 이 행을 참조하는 테이블", "F"},
		{"D", "staticEncrypt 칸 복호화 보기 (다시 D 원래 값)", "D"},
		{"+ / -", "테이블 보기 WHERE 에 칸 값 = / <> 더하기·빼기", "+"},
		{"q", "종료", "q"},
	}},
	{"JSON 값 보기 (o)", fResult, [][3]string{
		{"t", "나무 ↔ 원문", "t"},
		{"Enter / space", "접기 · 펴기", "enter"},
		{"y / Y", "값 / 경로($.a[0].b) 복사", "y"},
	}},
	{"Redis 값 편집", fResult, [][3]string{
		{"e / Enter", "값·항목 고치기", "e"},
		{"a · d", "항목 추가 · 지우기", "a"},
		{"t · r · D", "TTL · 이름 바꾸기 · 키 지우기", "t"},
		{"esc", "편집기 닫기", "esc"},
	}},
	{"마우스", fKeep, [][3]string{
		{"클릭", "영역 포커스 · 행·칸 선택 · 탭 전환", ""},
		{"끌기", "편집기 선택", ""},
		{"더블클릭 / ▸ 클릭", "펼치기 · 열기 · 단어 선택", ""},
		{"휠", "트리 · 편집기 · 결과 스크롤", ""},
		{"+ 클릭", "새 탭", ""},
		{"경계 끌기", "트리 폭 · 편집기 높이 (다음에도 그 크기)", ""},
		{"열 머리 클릭", "정렬 (테이블 보기는 서버 정렬, Shift 는 정렬 열 더하기)", ""},
		{"WHERE · ORDER BY 칸 클릭", "테이블 보기 조건·정렬 입력", ""},
	}},
}

type nodeKind int

type focus int

type overlay int

type runKind int

type ask int

type txAfter int

type node struct {
	kind     nodeKind
	label    string
	conn     int // db.Project.Conns 인덱스 (nGroup 은 -1)
	schema   string
	key      string // Redis: 키 전체 이름(nKey) 또는 접두어(nFolder)
	expanded bool
	loaded   bool
	gen      int // 자식 읽기 요청 세대. 늦게 온 이전 응답을 버린다
	children []*node
	parent   *node
}

type row struct {
	n     *node
	depth int
}

type childrenMsg struct {
	n        *node
	gen      int
	children []*node
	err      error
}

type resultMsg struct {
	tab   int // queryTab.id. 닫힌 탭의 결과는 버린다
	seq   int
	conn  int
	sql   string // 기록에 남길 글. 비면 남기지 않는다
	stmts int
	res   *db.Result
	err   error
	kind  runKind
	all   []*db.Result // 여러 문장·명령을 실행했을 때 문장마다의 결과
	key   string       // rkRedisWrite: 다시 읽을 키(지웠으면 "")
	old   string       // rkRedisWrite: 편집한 원래 키
	// 편집기 세션 실행인지. 풀에서 읽은 결과(운영 화면·구조·신호·가져오기)의 InTx 는 늘 false 라 트랜잭션 표시를 지우면 안 된다
	session bool
	rev     int               // 실행을 시작할 때 결과 표의 편집 판. 그사이 고친 편집을 결과로 덮거나 지우지 않는다
	start   time.Time         // 실행을 시작한 때. 긴 실행 알림(R30)이 잰다
	changes *db.ChangeSet     // rkSubmit: 낸 묶음. 성공하면 되돌리기 묶음을 쌓는다(R50)
	undo    *undoEntry        // rkSubmit: 되돌리기로 낸 묶음. 성공하면 탭에서 뺀다
	params  map[string]string // 파라미터 실행의 이름 → 값. 성공하면 기억한다(R55)
}

// redisValueMsg 는 값 편집기에 넣을 키 하나를 읽은 결과다.
type redisValueMsg struct {
	tab  int
	conn int
	v    *db.RedisValue
	err  error
}

// txDoneMsg 는 열린 트랜잭션 확인 창에서 고른 COMMIT·ROLLBACK 이 끝난 결과다. done 은 끝낸 접속이다.
type txDoneMsg struct {
	prompt txPrompt
	done   []int
	err    error
}

// tickMsg 는 헤더의 트랜잭션 시계를 1초마다 다시 그리게 한다.
type tickMsg struct{}

type catalogMsg struct {
	conn int
	gen  int // model.catGen 세대. 다르면 늦게 온 이전 요청이라 버린다
	cat  *complete.Catalog
	err  error
}

type ddlMsg struct {
	conn  int
	table string
	ddl   string
	err   error
}

// statusMsg 는 명령 고루틴(내보내기·기록)이 끝나고 상태 줄에 알릴 글이다.
type statusMsg string

// defMsg 는 정의로 이동(R26)에서 커서 아래 이름을 찾은 결과다.
type defMsg struct {
	tab, conn int
	name      string
	obj       db.Object
	err       error
}

// pickListMsg 는 고르기 창에 띄울 목록(데이터베이스·스키마)을 읽은 결과다. do 는 고른 항목으로 부른다.
type pickListMsg struct {
	conn   int
	prompt string
	items  []string
	err    error
	do     func(string) tea.Cmd
}

// dbSwitchMsg 는 접속을 다른 데이터베이스로 다시 열려고 옛 풀·세션을 닫은 결과다(R27.1).
type dbSwitchMsg struct {
	conn     int
	db, prev string
	err      error
}

// searchPathMsg 는 편집기 세션에 search_path 를 보낸 결과다(R27.2).
type searchPathMsg struct {
	conn   int
	schema string
	err    error
}

// searchCountMsg 는 prod 데이터 검색 전에 볼 테이블 수를 센 결과다(R33.2). open 은 y 뒤에 결과 탭을 골라 검색을 시작한다.
type searchCountMsg struct {
	conn int
	n    int
	err  error
	open func() tea.Cmd
}

// exportTickMsg 는 전체 내보내기 중 받은 바이트를 상태 줄에 다시 그리게 한다.
type exportTickMsg struct{ job *exportJob }

// exportDoneMsg 는 전체 내보내기(R33.1)가 끝난 결과다.
type exportDoneMsg struct {
	job  *exportJob
	rows int64
	path string
	err  error
}

// exportJob 은 진행 중인 전체 내보내기다. bytes 는 COPY 고루틴이 쓰고 화면이 읽는다.
type exportJob struct {
	cancel context.CancelFunc
	// 사용자가 취소했다(esc·DB 바꾸기). 취소 뒤의 오류는 드라이버마다 context.Canceled 로 오지 않아 이 표시로 가른다. 화면 고루틴만 읽고 쓴다
	canceled bool
	bytes    atomic.Int64
	start    time.Time
	conn     int
}

// paletteAct 는 명령 팔레트(R40) 항목이 누를 키와 그 전에 옮길 포커스다.
type paletteAct struct {
	at focus
	k  tea.KeyMsg
}

// paletteTable 은 명령 팔레트의 테이블·뷰 항목이다(R40.2). quiet 면 거르기 글이 있을 때만 보인다.
type paletteTable struct {
	it    finder.Item
	conn  int
	quiet bool
}

// paletteRecent 는 빈 팔레트 맨 위에 보일 최근 테이블 수다(R40.2)
const paletteRecent = 10

// paletteEnvs 는 거를 때 최근이 아닌 테이블을 둘 환경 순서다. 쓰기 위험이 큰 prod 를 맨 뒤에 둔다
var paletteEnvs = []string{"local", "stg", "prod"}

// fkMsg 는 결과 표의 F·R(R36)로 외래 키를 읽은 결과다. db 는 읽기 시작할 때의 데이터베이스다.
type fkMsg struct {
	conn   int
	db     string
	follow grid.Follow
	fks    []db.FK
	err    error
}

// estimateMsg 는 쓰기 확인 창에 보일 영향 행 수 추정(R39)이다. id 가 지금 확인 창과 다르면 이전 확인의 것이라 버린다.
type estimateMsg struct {
	id   int
	rows int64
	err  error
}

// keyScanMsg 는 Redis 패턴 일괄 삭제(R42)에서 지울 키를 SCAN 한 결과다.
type keyScanMsg struct {
	conn      int
	pattern   string
	keys      []string
	truncated bool
	err       error
}

// unlinkTickMsg 는 키 일괄 삭제 중 지운 수를 상태 줄에 다시 그리게 한다.
type unlinkTickMsg struct{ job *unlinkJob }

// unlinkDoneMsg 는 키 일괄 삭제(R42)가 끝난 결과다.
type unlinkDoneMsg struct {
	job *unlinkJob
	n   int
	err error
}

// unlinkJob 은 진행 중인 키 일괄 삭제다. done 은 UNLINK 고루틴이 쓰고 화면이 읽는다. canceled 는 화면 고루틴만 읽고 쓴다
type unlinkJob struct {
	cancel   context.CancelFunc
	canceled bool
	done     atomic.Int64
	total    int
	conn     int
}

// yesNo 는 실행이 아닌 확인(즐겨찾기 덮어쓰기·지우기, prod 데이터 검색, 키 일괄 삭제)이다. 확인 창(oConfirm)을 같이 쓴다.
type yesNo struct {
	q       string
	do      func() tea.Cmd
	preview []string // 결과 영역에 보일 줄(지울 키)
}

// pendingRun 은 실행 하나다. prod 쓰기와 EXPLAIN ANALYZE 쓰기는 확인(명세 R6.1·R9)을 기다린다.
type pendingRun struct {
	tab       int
	conn      int
	readOnly  bool
	analyze   bool // 문장을 실제로 실행하는 EXPLAIN ANALYZE 라 환경과 상관없이 먼저 묻는다
	sql       string
	stmts     int
	exec      func(ctx context.Context) (*db.Result, error)
	kind      runKind
	confirm   string   // 환경과 상관없이 묻는 글(제출·가져오기·백엔드 신호). prod 면 prod 이름을 앞에 붙인다
	danger    string   // 위험 문장 이유(R25). 환경과 상관없이 묻고, prod 쓰기 확인과 한 창에 같이 보인다
	preview   []string // 확인 창에 보일 줄(제출할 SQL, 가져오기 매핑)
	tx        bool     // 편집기 세션 실행이라 Manual 모드면 BEGIN 을 먼저 보낸다
	discardOK bool     // 보류 중인 편집을 버려도 된다고 답했다
	all       *[]*db.Result
	key, old  string           // rkRedisWrite
	restore   *tableview.Model // 확인을 거절하면 되돌릴 테이블 보기 상태
	// 시작할 때 탭 접속을 conn 으로 바꾼다(트리에서 연 객체). 확인을 거절하면 탭이 그대로 남는다
	retarget bool
	tv       *tableview.Model  // 시작할 때 탭에 붙일 테이블 보기
	title    string            // tv 와 함께 붙일 탭 이름
	ask      int               // 확인 창 번호(model.confirmSeq). 늦게 온 영향 행 수 추정을 가른다
	estimate string            // 확인 창에 같이 보일 영향 행 수 추정(R39). 못 받았으면 비어 있다
	note     string            // 시작할 때 상태 줄 앞에 붙일 글(enc() 확장, R52)
	changes  *db.ChangeSet     // rkSubmit 이 낸 묶음(R50)
	undo     *undoEntry        // rkSubmit 이 되돌리기 묶음을 낸다(R50)
	params   map[string]string // 파라미터 실행의 이름 → 값(R55)
}

// paramState 는 :name 자리마다 값을 묻는 중인 실행이다. run.exec 는 값을 다 받은 뒤 만든다.
type paramState struct {
	run   pendingRun
	sql   string // :name 을 $n 으로 바꾼 문장
	names []string
	vals  []string
	prev  map[string]string // 이 프로젝트에서 이름마다 마지막으로 성공한 값(R55)
}

// txPrompt 는 열린 트랜잭션을 끝내야 할 수 있는 동작(탭 닫기·종료·접속 바꾸기)이 기다리는 확인이다.
type txPrompt struct {
	tab   int // 확인을 띄운 탭. 끝나기 전에 다른 탭으로 옮겨도 이 탭에 동작한다
	conns []int
	after txAfter
	node  *node       // afterActivate: 다시 열 트리 노드
	run   *pendingRun // afterRun: 접속을 바꿔 시작할 실행
	db    string      // afterDB: 다시 열 데이터베이스
}

// subResult 는 결과 영역 위 작은 탭 하나다. 고정한 결과이거나 여러 문장 실행의 문장별 결과다.
type subResult struct {
	title  string
	res    *db.Result
	pinned bool
}

// script 는 한 세션에서 차례로 돌릴 문장들이다. Redis 는 줄 하나가 명령 하나다.
type script struct {
	conn  db.Conn
	stmts []string      // PostgreSQL 문장
	cmds  [][]string    // Redis 명령 인자. 따옴표 안 공백이 든 인자를 다시 나누지 않게 나눈 채로 둔다
	args  []any         // 문장 하나에 바인딩할 값(:name 파라미터)
	out   *[]*db.Result // 있으면 문장마다의 결과를 싣는다
	// 문장마다 READ ONLY 트랜잭션 안에서 돌린다. 잠긴 prod 의 읽기 판정 문장에 켜서 판정이 놓친 쓰기를 서버가 거절하게 한다(R54)
	readOnlyTx bool
}

// pinnedFirst 는 고정한 탭(R57)을 앞으로 모으는 안정 정렬 비교다. 같은 무리 안 순서는 그대로다.
func pinnedFirst(a, b *queryTab) int {
	switch {
	case a.pinned == b.pinned:
		return 0
	case a.pinned:
		return -1
	}
	return 1
}

// queryTab 은 쿼리 버퍼 하나다. 편집기와 결과 표를 탭이 가진다.
type queryTab struct {
	id              int
	title           string
	editor          editor.Model
	conn            int // 이 탭의 활성 접속, 없으면 -1
	file            string
	res             *db.Result
	resErr          string
	summary         string // 마지막 결과 요약. 결과 영역 제목 줄에 보인다
	grid            grid.Model
	seq             int // 이 탭이 마지막으로 시작한 실행. 다른 seq 의 결과는 늦게 온 이전 결과다
	running         context.CancelFunc
	runningReadOnly bool
	cancelled       bool

	tv       *tableview.Model // 테이블 보기 중이면 그 상태
	last     *pendingRun      // 마지막으로 표를 채운 읽기 실행. 제출·백엔드 신호 뒤 다시 돌려 새로 고친다
	subs     []subResult
	view     int // 보이는 결과: -1 이면 마지막 결과(res), 아니면 subs 인덱스
	plan     *plantree.Model
	planOn   bool
	redis    *redisview.Model // Redis 값 편집기가 열려 있으면 그 모델
	redisKey string           // 값 편집기가 연 키
	viewConn int              // 결과·테이블 보기·값 편집기를 만든 접속. 탭 접속과 다르면 그 화면에서 쓰기·다시 읽기를 막는다
	viewDB   string           // viewConn 의 그때 데이터베이스(R27.1). B 로 바뀌었으면 같은 접속이어도 막는다
	gridH    int              // 결과 표에 준 높이. 위에 붙는 줄 수가 바뀔 때만 다시 맞춘다
	// 지금 표를 채운 성공한 실행의 문장과 접속. 전체 내보내기(R33.1)가 다시 보낸다. 쓰기 문장도 남겨 F7 이 거절하게 한다
	exportSQL  string
	exportConn int
	// 연 파일을 마지막으로 읽거나 쓴 내용과 편집 판(R45.2). 디스크가 이 내용과 달라지면 autoStop 으로 자동 저장을 멈춘다
	diskText string
	editRev  int
	autoStop bool
	// 사용자가 편집기에서 글을 고쳤다. 끝낼 때 파일 없는 탭은 이 탭만 새 쿼리로 저장한다(R45.1). SetValue 로 넣은 글은 세지 않는다
	userText bool
	// 세션을 되살릴 때 파일이 바뀌어 갈라 둔 `(저장 안 한 글)` 탭의 원래 파일. 끝낼 때 그 옆 충돌 사본에 저장한다
	conflictOf string
	name       string       // 사용자가 바꾼 탭 이름(R57). 비면 제목·파일 이름이다
	pinned     bool         // 고정한 탭(R57). 탭 줄 앞에 모이고 닫을 때 묻는다
	undo       []*undoEntry // 제출 되돌리기 묶음, 마지막이 가장 최근(R50)
}

// completer 는 편집기 자동완성 함수다. 탭의 접속이 바뀌어도 부를 때 다시 읽도록 탭을 가리킨다.
type completer struct {
	m *model
	t *queryTab
}

// layout 은 화면 영역의 안쪽 크기다. View 와 마우스 좌표 계산이 같은 값을 쓴다.
// 세로: 헤더 1줄, 트리(테두리 포함 bodyH), 오른쪽은 탭 줄 1 + 편집기 edH+2 + 결과 resH+2, 상태 줄 1줄.
type layout struct {
	treeW, rightW           int
	bodyH, treeH, edH, resH int
}

type click struct {
	row int
	at  time.Time
}

// border 는 마우스로 끄는 영역 경계다(R46.1).
type border int

const (
	bNone  border = iota
	bTree         // 트리와 오른쪽 사이 세로 경계
	bSplit        // 편집기와 결과 사이 가로 경계
)

type model struct {
	proj    *db.Project
	clients *db.Clients
	life    context.Context // 종료하면 끝나는 컨텍스트. 트리 조회·실행이 모두 여기서 갈라진다
	stop    context.CancelFunc
	roots   []*node
	rows    []row
	cursor  int
	treeTop int
	filter  string

	focus focus
	input textinput.Model

	tabs   []*queryTab
	tab    int // 활성 탭 인덱스
	tabSeq int // 탭 id 와 "Query N" 번호

	patterns map[int]string    // Redis 접속별 SCAN 패턴
	inTx     map[int]bool      // 접속별 편집기 세션이 마지막 결과에서 트랜잭션 안이었는지
	manual   map[int]bool      // 접속별 Manual 트랜잭션 모드
	txSince  map[int]time.Time // 접속별 트랜잭션이 열린 때(헤더 시계)
	ticking  bool
	// 접속별 자동완성 목록. 키가 있으면 읽는 중이거나 읽었다(실패하면 nil 로 남아 r 로 다시 읽는다)
	catalogs map[int]*complete.Catalog
	catGen   map[int]int    // 접속별 자동완성 목록 요청 세대
	hist     *history.Store // 열지 못했으면 nil
	seq      int
	status   string

	overlay      overlay
	pickerItems  []string
	pickerLower  []string                // pickerItems 의 소문자 사본. 거를 때마다(키·그리기) 다시 만들지 않게 열 때 한 번 만든다
	pickerConn   map[string]string       // 항목 → 접속 uuid. 고르기 창에 → 접속 이름으로 보인다
	pickerTables map[string]paletteTable // 명령 팔레트의 테이블 항목(R40.2). 그릴 때 [환경] 에 색을 입힌다
	pickerCursor int
	pickDo       func(string) tea.Cmd // nil 이면 queries 파일 열기(Ctrl+O)
	pickDel      func(string) tea.Cmd // 즐겨찾기 고르기의 지우기(d)
	promptDo     func(string) tea.Cmd // oPrompt 의 Enter
	yes          *yesNo
	histCursor   int
	pending      *pendingRun
	previewTop   int
	params       *paramState
	txAsk        *txPrompt
	finder       finder.Model
	jump         *finder.Item // 찾기에서 고른 객체. 부모를 읽는 중이면 읽은 뒤 다시 찾는다
	jumpConn     int          // jump 의 접속 인덱스
	importNode   *node        // CSV 가져오기 대상 테이블
	revealTab    int          // 찾기를 이어 가는 대상이 정의로 이동(R26)이면 요청한 탭 id(0 은 아님). 테이블 보기 대신 그 탭에 구조를 연다

	dbOver   map[int]string // 접속별로 B 로 바꾼 데이터베이스(R27.1). .idea 는 고치지 않는다
	dbOrig   map[int]string // 처음 B 로 바꾸기 전 접속 파일의 DB. 접속을 다시 읽을 때 파일의 DB 를 고쳤는지 가린다(R59)
	schemaOf map[int]string // 접속별로 F6 으로 고른 스키마(R27.2)
	export   *exportJob     // 진행 중인 전체 내보내기
	// 데이터베이스를 바꾸는 중(R27.1). 끝나기 전의 두 번째 B 는 막는다
	switching bool

	confirmSeq int        // 실행 확인 창을 띄울 때마다 올린다(R39 추정 짝 맞추기)
	unlink     *unlinkJob // 진행 중인 Redis 키 일괄 삭제(R42)
	boot       tea.Cmd    // 시작할 때 돌릴 명령(되살린 탭의 자동완성 목록 읽기)
	saved      bool       // 끝낼 때 세션을 이미 저장했다(R41)
	saveErr    error      // 세션 저장 오류. 화면을 닫은 뒤 main 이 알린다
	quitNotes  []string   // 끝낼 때 남긴 충돌 사본(R45.3)·쓰지 못한 탭. 화면을 닫은 뒤 main 이 알린다
	restoring  bool       // 세션을 되살리는 중이다. 탭을 만들고 옮기는 사이 자동 저장하지 않는다
	// 세션 파일에서 읽은 이 프로젝트의 상태. 최근 파일(R44)·창 크기(R46.2)를 여기 두고 저장할 때 탭만 지금 것으로 바꾼다
	sess session.State
	// 접속 인덱스 → 그 접속에 붙은 queries 파일(최근 수정 순), -1 은 어느 접속에도 붙지 않은 파일(R43)
	qfiles map[int][]string
	qstale bool // 자동 저장으로 쓴 파일이 있어 qfiles 순서가 낡았다. 탭을 옮기거나 닫을 때 다시 읽는다
	mac    bool // 키 안내를 ⌘ 글자로 보인다(R47.1)

	// prod 쓰기 잠금(R54). prodLock 은 켤 때 늘 참이고 v6 이전 시험만 끈다. unlocked 는 접속별로 연 마감 시각이다
	prodLock    bool
	unlocked    map[int]time.Time
	lockTicking bool
	// 접속별 사용자 타입(OID 숫자) → enum 값(R49.2). enum 이 아니면 빈 목록, 읽는 중이면 nil 이다
	enums map[int]map[string][]string
	long  *longEdit // 칸 값 전체 화면 편집기(R49.3)
	// 접속 추가·고치기 창(R59.2)
	connForm *connForm

	treeW     int
	edH       int // 편집기 높이(줄). 0 이면 본문의 defaultEditorRatio%
	lastClick click
	drag      border // 끄는 중인 경계
	dragOff   int    // 끌기 시작한 자리와 경계 크기의 차이. 경계를 잡은 칸이 마우스를 따라가게 한다
	dragFrom  int    // 끌기 시작할 때 저장한 크기. 그려진 크기로 되돌아오면 이 값으로 돌린다(작은 창에서 줄여 그린 크기를 저장하지 않게)
	dragBase  int    // 끌기 시작할 때 그려진 크기
	paneRev   int    // 키보드로 바꾼 창 크기의 판. 마지막 키 뒤 paneSaveDelay 가 지나면 한 번만 저장한다(R46.2)

	w, h int
}

func newModel(p *db.Project) *model {
	life, stop := context.WithCancel(context.Background())
	m := &model{proj: p, clients: db.NewClients(), life: life, stop: stop, input: textinput.New(),
		patterns: map[int]string{}, inTx: map[int]bool{}, catalogs: map[int]*complete.Catalog{}, catGen: map[int]int{},
		manual: map[int]bool{}, txSince: map[int]time.Time{}, dbOver: map[int]string{}, dbOrig: map[int]string{}, schemaOf: map[int]string{},
		treeW: defaultTreeWidth, mac: keys.MacStyle(false),
		prodLock: true, unlocked: map[int]time.Time{}, enums: map[int]map[string][]string{}}
	m.roots = connRoots(p.Conns, nil, nil)
	if len(p.Warnings) > 0 {
		m.status = "건너뜀: " + strings.Join(p.Warnings, ", ")
	}
	m.newTab()
	m.refreshQueries()
	return m
}

func (m *model) Init() tea.Cmd { return m.boot }

func (n *node) expandable() bool {
	return n.kind == nGroup || n.kind == nConn || n.kind == nSchema || n.kind == nFolder || n.kind == nQueries
}

// ─── 탭 ───

// newTab 은 현재 탭의 접속을 이어받은 빈 탭을 만들고 활성으로 바꾼다.
func (m *model) newTab() *queryTab {
	conn := -1
	if len(m.tabs) > 0 {
		conn = m.cur().conn
	}
	// 새 탭으로 옮길 때도 떠나는 탭을 자동 저장한다(R45.2)
	if len(m.tabs) > 0 {
		m.autoSaveStatus(m.cur())
		m.refreshStale()
	}
	m.tabSeq++
	t := &queryTab{id: m.tabSeq, title: fmt.Sprintf("Query %d", m.tabSeq), editor: editor.New(), grid: grid.New(), view: -1, viewConn: -1}
	t.editor.SetCompleter(completer{m: m, t: t}.suggest)
	t.editor.SetTemplates(editor.DefaultTemplates())
	m.setConn(t, conn) // 이어받은 접속의 목록은 이미 읽었거나 읽는 중이다
	m.tabs = append(m.tabs, t)
	m.tab = len(m.tabs) - 1
	m.resize()
	m.setFocus(m.focus)
	return t
}

func (m *model) cur() *queryTab { return m.tabs[m.tab] }

func (m *model) findTab(id int) *queryTab {
	for _, t := range m.tabs {
		if t.id == id {
			return t
		}
	}
	return nil
}

func (m *model) switchTab(i int) {
	if i < 0 || i >= len(m.tabs) {
		return
	}
	// 탭을 옮길 때 떠나는 탭을 자동 저장한다(R45.2). 닫는 탭은 closeTab 이 지우기 전에 저장한다
	if i != m.tab {
		m.autoSaveStatus(m.cur())
		m.refreshStale()
	}
	m.tab = i
	m.setFocus(m.focus)
}

// setConn 은 탭의 접속을 바꾸고 편집기 언어를 맞춘다. PostgreSQL 이면 자동완성 목록을 처음 한 번 읽는 명령을 돌려준다.
func (m *model) setConn(t *queryTab, i int) tea.Cmd {
	t.conn = i
	lang := editor.LangSQL
	if i >= 0 && m.conn(i).Kind == db.KindRedis {
		lang = editor.LangRedis
	}
	t.editor.SetLanguage(lang)
	return m.ensureCatalog(i)
}

// bound 는 탭이 접속에 묶인 화면(테이블 보기·값 편집기·보류 중인 편집)을 들고 있는지다.
// 다른 접속의 객체는 이 탭을 바꾸지 않고 새 탭에 연다.
func (t *queryTab) bound() bool {
	return t.tv != nil || t.redis != nil || t.grid.Pending() > 0
}

// staleViewDB 는 결과를 만든 접속을 고치거나 지웠다는 viewDB 표시다(R59). 어느 데이터베이스 이름과도 같지 않다.
const staleViewDB = "\x00"

// staleView 는 보이는 결과·테이블 보기·값 편집기를 만든 접속이 탭 접속과 다르거나 그 뒤 데이터베이스를 바꿨으면(B) 알리고 참이다.
// 다른 접속·DB 의 pid·편집·키를 지금 접속(prod 일 수 있다)에 보내지 않게 쓰기·다시 읽기 앞에서 부른다.
func (m *model) staleView(t *queryTab) bool {
	if t.viewConn < 0 {
		return false
	}
	if t.viewDB == staleViewDB {
		m.status = "보이는 결과는 접속을 고치기 전의 것이라 막음 (다시 실행하거나 새로 여세요)"
		return true
	}
	if t.viewConn == t.conn {
		if t.viewDB == m.conn(t.conn).DB {
			return false
		}
		m.status = "보이는 결과는 데이터베이스를 바꾸기 전의 것이라 막음 (다시 실행하거나 새로 여세요)"
		return true
	}
	cur := "없음"
	if t.conn >= 0 {
		cur = m.conn(t.conn).Name
	}
	m.status = fmt.Sprintf("보이는 결과는 %s 접속의 것이라 막음 (탭 접속 %s, 그 접속으로 돌아가거나 새로 여세요)", m.conn(t.viewConn).Name, cur)
	return true
}

// tabLabels 는 탭 줄의 글자다. 마우스가 같은 폭으로 클릭 위치를 찾는다.
func (m *model) tabLabels() []string {
	out := make([]string, len(m.tabs))
	for i, t := range m.tabs {
		name := textutil.Printable(t.displayName(), false)
		if t.pinned {
			name = "📌" + name
		}
		if t.running != nil {
			name += " …"
		}
		out[i] = fmt.Sprintf(" %d %s ", i+1, name)
	}
	return out
}

// displayName 은 탭 줄에 보일 이름이다. 바꾼 이름(R57), 연 파일 이름, 제목 순이다.
func (t *queryTab) displayName() string {
	switch {
	case t.name != "":
		return t.name
	case t.file != "":
		return path.Base(t.file)
	}
	return t.title
}

// ─── 자동완성 ───

// ensureCatalog 는 PostgreSQL 접속의 자동완성 목록을 아직 요청하지 않았으면 읽는 명령을 돌려준다.
func (m *model) ensureCatalog(i int) tea.Cmd {
	if i < 0 || m.conn(i).Kind != db.KindPG {
		return nil
	}
	if _, ok := m.catalogs[i]; ok {
		return nil
	}
	m.catalogs[i] = nil
	// 요청마다 세대를 올려 늦게 온 이전 요청(DB 바꾸기·새로고침 전)의 목록이 비운 자리를 되살리지 않게 한다
	m.catGen[i]++
	gen := m.catGen[i]
	return withPG(m.life, m.clients, m.conn(i), func(ctx context.Context, pg *db.PGConn, err error) tea.Msg {
		if err != nil {
			return catalogMsg{conn: i, gen: gen, err: err}
		}
		cat, err := pg.LoadCatalog(ctx)
		return catalogMsg{conn: i, gen: gen, cat: cat, err: err}
	})
}

// suggest 는 편집기 Update 안(화면 고루틴)에서 불려 모델을 잠금 없이 읽는다.
func (c completer) suggest(text string, offset int) []editor.Item {
	q := complete.Query{Text: text, Offset: offset}
	var items []complete.Item
	if c.t.conn >= 0 && c.m.conn(c.t.conn).Kind == db.KindRedis {
		// 키 후보는 트리에서 그 접속 아래 지금 읽혀 있는 키다
		var keys []string
		if cn := c.m.connNode(c.t.conn); cn != nil {
			keys = redisKeys(cn)
		}
		items = complete.SuggestRedis(q, keys)
	} else {
		// 목록을 아직 못 읽었으면 nil 이고, 그때는 키워드·함수만 나온다
		items = complete.SuggestSQL(c.m.catalogs[c.t.conn], q)
	}
	out := make([]editor.Item, len(items))
	for i, it := range items {
		out[i] = editor.Item(it)
	}
	return out
}

// ─── 화면 크기 ───

func (m *model) layout() layout {
	l := layout{bodyH: m.h - 2}
	l.treeW = m.clampTree(m.treeW)
	l.rightW = max(10, m.w-l.treeW-4)
	l.treeH = max(1, l.bodyH-2)
	l.edH = m.clampEditor(m.edH)
	l.resH = max(1, l.bodyH-5-l.edH)
	return l
}

// clampTree 는 트리 폭을 20칸~화면 폭의 60%(오른쪽 영역 최소 폭은 남긴다)로 맞춘다(R46.1).
// 저장한 크기는 그대로 두고 그릴 때 맞춰, 창을 다시 키우면 원래 크기로 돌아온다.
func (m *model) clampTree(w int) int {
	hi := max(minTreeWidth, min(m.w*maxTreePercent/100, m.w-4-minRightWidth))
	return max(minTreeWidth, min(w, hi))
}

// clampEditor 는 편집기 높이를 3줄~(편집기·결과 몫 - 3줄)로 맞춘다. 0 이면 기본 비율이다.
func (m *model) clampEditor(h int) int {
	avail := m.h - 2 - 5 // 헤더·상태 줄, 탭 줄, 편집기·결과 테두리
	if h <= 0 {
		h = avail * defaultEditorRatio / 100
	}
	return max(1, min(max(h, minEditorLines), avail-minEditorLines))
}

// resize 는 창 크기·영역 비율이 바뀌면 모든 탭의 편집기·결과 표 크기를 맞춘다.
func (m *model) resize() {
	l := m.layout()
	for _, t := range m.tabs {
		t.editor.SetSize(l.rightW, l.edH)
		t.grid.SetSize(l.rightW, l.resH)
		t.gridH = -1 // 표 위 줄 수를 빼는 건 그릴 때 한다
	}
	m.input.Width = max(10, m.w-20)
}

// ─── 트리 ───

func (m *model) rebuild() {
	m.rows = m.rows[:0]
	m.walk(m.roots, 0)
	m.cursor = clamp(m.cursor, len(m.rows))
}

func (m *model) walk(ns []*node, depth int) {
	for _, n := range ns {
		if m.filter != "" && !matches(n, strings.ToLower(m.filter)) {
			continue
		}
		m.rows = append(m.rows, row{n, depth})
		if n.expanded {
			m.walk(n.children, depth+1)
		}
	}
}

// 거르기 중에는 자신이나 자손이 맞는 노드만 남긴다.
func matches(n *node, f string) bool {
	if strings.Contains(strings.ToLower(n.label), f) {
		return true
	}
	for _, c := range n.children {
		if matches(c, f) {
			return true
		}
	}
	return false
}

func (m *model) current() *node {
	if m.cursor >= 0 && m.cursor < len(m.rows) {
		return m.rows[m.cursor].n
	}
	return nil
}

func (m *model) conn(i int) db.Conn { return m.proj.Conns[i] }

func (m *model) isRedisKeyStage(n *node) bool {
	return n.conn >= 0 && m.conn(n.conn).Kind == db.KindRedis && (n.kind == nConn || n.kind == nKey || n.kind == nFolder)
}

func (m *model) selectNode(target *node) {
	for i, r := range m.rows {
		if r.n == target {
			m.cursor = i
		}
	}
}

func (m *model) setFocus(f focus) {
	m.focus = f
	for _, t := range m.tabs {
		t.editor.Blur()
		// 결과 영역을 떠난 뒤 q·글자 키가 보이지 않는 거르기 입력으로 새지 않게 한다
		if f != fResult {
			t.grid.StopTyping()
		}
	}
	if f == fEditor {
		m.cur().editor.Focus()
	}
}

// expand 는 노드를 펼치고, 아직 안 읽었으면 자식을 읽는 명령을 돌려준다.
// 트리 노드는 모델이 가진 상태라 여기서 바꾼다. 명령 고루틴은 노드를 읽기만 하고 결과는 Update 에서 붙인다.
func (m *model) expand(n *node) tea.Cmd {
	n.expanded = true
	if n.loaded || n.kind == nGroup {
		m.rebuild()
		return nil
	}
	n.gen++
	gen := n.gen
	n.children = []*node{{kind: nInfo, label: "불러오는 중…", conn: n.conn, parent: n}}
	m.rebuild()
	c := m.conn(n.conn)
	pattern := m.patterns[n.conn]
	if pattern == "" {
		pattern = "*"
	}
	kind, label, idx, clients, life := n.kind, n.label, n.conn, m.clients, m.life
	return func() tea.Msg {
		ctx, cancel := context.WithTimeout(life, loadTimeout)
		defer cancel()
		var out []*node
		switch {
		case kind == nConn && c.Kind == db.KindPG:
			pg, err := clients.PG(ctx, c)
			if err != nil {
				return childrenMsg{n: n, gen: gen, err: err}
			}
			ss, err := pg.FindSchemas(ctx)
			if err != nil {
				return childrenMsg{n: n, gen: gen, err: err}
			}
			for _, s := range ss {
				out = append(out, &node{kind: nSchema, label: s, conn: idx, parent: n})
			}
		case kind == nSchema:
			pg, err := clients.PG(ctx, c)
			if err != nil {
				return childrenMsg{n: n, gen: gen, err: err}
			}
			ts, err := pg.FindTables(ctx, label)
			if err != nil {
				return childrenMsg{n: n, gen: gen, err: err}
			}
			for _, t := range ts {
				out = append(out, &node{kind: nTable, label: t, schema: label, conn: idx, parent: n})
			}
		case kind == nConn && c.Kind == db.KindRedis:
			rc, err := clients.Redis(ctx, c)
			if err != nil {
				return childrenMsg{n: n, gen: gen, err: err}
			}
			keys, more, err := rc.GetKeys(ctx, pattern)
			if err != nil {
				return childrenMsg{n: n, gen: gen, err: err}
			}
			out = redisNodes(redisview.BuildTree(keys), n)
			if more {
				out = append(out, &node{kind: nInfo, label: "… 더 있음 (/ 로 패턴 좁히기)", conn: idx, parent: n})
			}
		}
		if len(out) == 0 {
			out = []*node{{kind: nInfo, label: "(비어 있음)", conn: idx, parent: n}}
		}
		return childrenMsg{n: n, gen: gen, children: out}
	}
}

// activate 는 트리 노드를 연다. 키(Enter·l)와 마우스(더블클릭·▸ 클릭)가 같이 쓴다.
// toggle 이면 이미 펼친 노드를 접는다.
func (m *model) activate(n *node, toggle bool) tea.Cmd {
	if n.kind == nFile {
		cmd := m.openFile(n.key)
		m.setFocus(fEditor)
		return cmd
	}
	t := m.cur()
	other := n.conn >= 0 && n.conn != t.conn
	if n.kind == nTable {
		// 쓰던 글·연결된 파일·다른 접속의 화면을 덮지 않게 새 탭에서 연다(Ctrl+O·기록과 같다)
		if t.file != "" || t.editor.Value() != "" || (other && t.bound()) {
			t = m.newTab()
		}
		return m.openTable(t, n.conn, db.TableRef{Schema: n.schema, Table: n.label}, "")
	}
	if n.kind == nKey && other && t.bound() {
		t = m.newTab()
	}
	// 이 탭만 쓰던 접속의 트랜잭션을 접속을 바꾸며 몰래 버리지 않는다
	if conns := m.orphanTx(t); other && len(conns) > 0 {
		return m.askTx(txPrompt{tab: t.id, conns: conns, after: afterActivate, node: n})
	}
	// 그 접속을 쓰는 탭이 없었으면 최근 파일을 연다(R44). 접속 줄을 고른 것만 센다(접기·스키마·키·이름공간 줄은 뺀다)
	fresh := n.kind == nConn && !(n.expanded && toggle) &&
		!slices.ContainsFunc(m.tabs, func(o *queryTab) bool { return o.conn == n.conn })
	var cat tea.Cmd
	if n.conn >= 0 {
		cat = m.setConn(t, n.conn)
	}
	if fresh {
		cat = tea.Batch(cat, m.autoOpen(n.conn))
	}
	if n.kind == nKey {
		return tea.Batch(cat, m.loadRedisValue(t, n.key))
	}
	if !n.expandable() {
		return cat
	}
	if n.expanded && toggle {
		n.expanded = false
		m.rebuild()
		return cat
	}
	return tea.Batch(cat, m.expand(n))
}

// openTable 은 탭 t 에 테이블 보기(R16)를 연다. where 가 있으면 그 WHERE 로 연다(R36 외래 키 따라가기).
// 추정 행 수를 먼저 읽고, 그 결과가 오면 첫 페이지를 읽는다. 탭의 접속·테이블 보기는 확인을 다 지나 시작할 때 바꾼다.
// 여는 길(트리·찾기·팔레트·외래 키·정의로 이동)이 모두 여기를 지난다. 최근 테이블(R40.3)은 확인을 지나 보기가 시작될 때(start) 남긴다.
func (m *model) openTable(t *queryTab, conn int, ref db.TableRef, where string) tea.Cmd {
	tv := tableview.New(ref, tableview.Options{})
	if where != "" {
		tv, _ = tv.ToggleWhere(where)
	}
	return m.guard(pendingRun{tab: t.id, conn: conn, readOnly: true, kind: rkEstimate, tx: true, retarget: true, tv: &tv, title: ref.Table,
		exec: execScript(m.clients, script{conn: m.conn(conn), stmts: []string{tv.EstimateSQL()}})})
}

// tableLabel 은 명령 팔레트의 테이블 항목 `[환경] 접속 · 스키마.테이블` 이다(R40.2).
func (m *model) tableLabel(conn int, schema, table string) string {
	c := m.conn(conn)
	return fmt.Sprintf("[%s] %s · %s.%s", c.Env(), c.Name, schema, table)
}

// keyMsgOf 는 키 이름(`ctrl+r`·`alt+e`·`F`)을 키 입력으로 바꾼다. 명령 팔레트가 도움말의 키 이름을 키 입력으로 되돌린다.
// bubbletea 가 이름 표를 내보내지 않아 키 종류 범위(음수는 특수 키, 0~127 은 제어 문자)에서 이름이 같은 첫 종류를 찾는다.
// 없으면 글자 키다.
func keyMsgOf(name string) tea.KeyMsg {
	alt := strings.HasPrefix(name, "alt+")
	name = strings.TrimPrefix(name, "alt+")
	for kt := tea.KeyType(-200); kt <= 200; kt++ {
		if (tea.Key{Type: kt}).String() == name {
			return tea.KeyMsg{Type: kt, Alt: alt}
		}
	}
	return tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(name), Alt: alt}
}

// globEscaper 는 Redis MATCH 패턴의 특수 글자를 글자 그대로 맞게 감싼다. 이름공간 접두어를 패턴에 넣을 때 쓴다(R42).
var globEscaper = strings.NewReplacer(`\`, `\\`, `*`, `\*`, `?`, `\?`, `[`, `\[`, `]`, `\]`)

// matchesAll 은 패턴이 `*`·`?` 만으로 되어 있고 `*` 가 하나 이상이라 (거의) 모든 키에 맞는지다(R42 FLUSHDB 경고).
// `?` 만이면 그 길이의 키에만 맞아 넣지 않는다.
func matchesAll(pattern string) bool {
	return strings.Contains(pattern, "*") && strings.Trim(pattern, "*?") == ""
}

// fkWhere 는 결과 행(테이블 열 → DB 값)에서 from 열 값을 꺼내 to 열의 WHERE 로 만든다(R36). 만들 수 없으면 그 이유를 돌려준다.
func fkWhere(row map[string]string, from, to []string, name string) (string, string) {
	vals := make([]string, len(from))
	for i, c := range from {
		v, ok := row[c]
		switch {
		case !ok:
			return "", "참조 열이 결과에 없음: " + c
		case v == "NULL":
			return "", "참조 값이 NULL"
		}
		vals[i] = v
	}
	where, err := db.FKWhere(to, vals)
	if err != nil {
		return "", "외래 키 열 수가 맞지 않음: " + name
	}
	return where, ""
}

// ─── 실행 ───

// target 은 실행할 글이다. 선택이 있으면 선택한 글, 없으면 커서의 문장(PostgreSQL)이나 줄(Redis)이다.
func (m *model) target(t *queryTab) string {
	text := t.editor.Value()
	if s, e, ok := t.editor.Selection(); ok {
		return text[s:e]
	}
	if t.conn >= 0 && m.conn(t.conn).Kind == db.KindRedis {
		return db.LineAt(text, t.editor.CursorOffset())
	}
	return db.StatementAt(text, t.editor.CursorOffset())
}

// run 은 현재 탭의 접속에서 text 를 실행한다. PostgreSQL 은 문장으로, Redis 는 줄로 나눠 한 세션에서 차례로 돌린다.
// 하나라도 읽기 전용이 아니면 prod 확인을 한 번 받는다. :name 자리가 있으면 값을 먼저 묻는다.
func (m *model) run(text string) tea.Cmd {
	t := m.cur()
	if t.conn < 0 {
		m.status = "트리에서 접속을 먼저 고르세요"
		return nil
	}
	c := m.conn(t.conn)
	s := script{conn: c, out: &[]*db.Result{}}
	readOnly := true
	var danger []string
	note := ""
	if c.Kind == db.KindPG {
		// enc('…')는 보낼 글에서만 암호문으로 바꾼다. 기록·자동 저장에는 확장 전 글(text)이 남는다(R52)
		sent, n, err := m.expandEnc(t.conn, text)
		if err != nil {
			m.status = err.Error()
			return nil
		}
		note = n
		s.stmts = db.SplitStatements(sent)
		for _, st := range s.stmts {
			readOnly = readOnly && db.IsReadOnlySQL(st)
		}
		if d := db.DangerOf(sent); d != "" {
			danger = append(danger, d)
		}
	} else {
		for _, ln := range strings.Split(text, "\n") {
			if args := db.SplitArgs(ln); len(args) > 0 {
				s.cmds = append(s.cmds, args)
				readOnly = readOnly && db.IsReadOnlyRedis(args)
				if d := db.DangerOfRedis(args); d != "" && !slices.Contains(danger, d) {
					danger = append(danger, d)
				}
			}
		}
	}
	n := len(s.stmts) + len(s.cmds)
	if n == 0 {
		m.status = "실행할 문장이 없음"
		return nil
	}
	s.readOnlyTx = readOnly && c.Kind == db.KindPG && m.prodLocked(t.conn)
	p := pendingRun{tab: t.id, conn: t.conn, readOnly: readOnly, sql: strings.TrimSpace(text), stmts: n,
		tx: c.Kind == db.KindPG, all: s.out, danger: strings.Join(danger, ", "), note: note}
	// 값을 다 묻고 나서 거절하지 않게 파라미터 창보다 먼저 본다(R54)
	if m.writeLocked(&p) {
		return nil
	}
	for _, st := range s.stmts {
		bound, names := params.Bind(st)
		if len(names) == 0 {
			continue
		}
		if n > 1 {
			m.status = "파라미터(:name·$n)가 있는 문장은 하나씩 실행하세요"
			return nil
		}
		// 이름마다 마지막으로 성공한 값(R55). 기억은 덤이라 읽지 못해도 빈 값으로 시작한다(깨진 파일은 paramstore 가 .bad 로 옮긴다)
		var prev map[string]string
		if path, err := paramstore.Path(); err == nil {
			prev, _ = paramstore.Load(path, m.proj.Dir)
		}
		m.params = &paramState{run: p, sql: bound, names: names, prev: prev}
		m.overlay = oParams
		m.input.Focus()
		m.nextParam()
		return nil
	}
	p.exec = execScript(m.clients, s)
	return m.guard(p)
}

// execScript 는 문장들을 접속 세션 하나에서 차례로 실행하고 첫 오류에서 멈춘다. 마지막 결과에 걸린 시간 합계를 싣는다.
func execScript(clients *db.Clients, s script) func(ctx context.Context) (*db.Result, error) {
	return func(ctx context.Context) (*db.Result, error) {
		// 탭 접속이 바뀐 뒤의 테이블 보기처럼 접속 종류와 다른 글이 오면 인덱스를 넘기 전에 멈춘다
		if (s.conn.Kind == db.KindPG) == (len(s.cmds) > 0) && len(s.stmts)+len(s.cmds) > 0 {
			return nil, fmt.Errorf("script does not match connection %s (%s)", s.conn.Name, s.conn.Kind)
		}
		var pg *db.PGConn
		var rc *db.RedisConn
		var err error
		if s.conn.Kind == db.KindPG {
			pg, err = clients.PG(ctx, s.conn)
		} else {
			rc, err = clients.Redis(ctx, s.conn)
		}
		if err != nil {
			return nil, err
		}
		n := len(s.stmts) + len(s.cmds)
		var res *db.Result
		var all []*db.Result
		var total time.Duration
		for i := range n {
			if pg != nil {
				res, err = pg.RunArgs(ctx, db.Query{SQL: s.stmts[i], Args: s.args, ReadOnly: s.readOnlyTx})
			} else {
				res, err = rc.Run(ctx, s.cmds[i])
			}
			if res != nil {
				total += res.Elapsed
			}
			if err != nil {
				if n > 1 {
					err = fmt.Errorf("statement %d of %d: %w", i+1, n, err)
				}
				return res, err
			}
			all = append(all, res)
		}
		if s.out != nil {
			*s.out = all
		}
		if res != nil {
			res.Elapsed = total
		}
		return res, nil
	}
}

// askOf 는 실행 전에 물어야 할 다음 단계다(명세 R6.1·R9·R15·R25). 위험 문장과 prod 쓰기는 한 창(askWrite)에서 같이 묻는다.
func (m *model) askOf(p *pendingRun) ask {
	t := m.findTab(p.tab)
	switch {
	case !p.discardOK && t != nil && replacesGrid[p.kind] && t.grid.Pending() > 0:
		return askDiscard
	case p.analyze:
		return askAnalyze
	case p.confirm != "" || p.danger != "" || (m.conn(p.conn).Env() == "prod" && !p.readOnly):
		return askWrite
	}
	return askNone
}

// guard 는 물어야 할 단계가 있으면 확인 창을 띄우고, 없으면 실행한다.
// prod 쓰기·위험 확인에 오른 편집기의 PostgreSQL UPDATE·DELETE 하나는 영향 행 수 추정을 따로 읽어 오면 확인 창에 더한다(R39).
// 확인 창은 추정을 기다리지 않고 바로 뜨며, 추정이 늦거나 실패해도 y/N 은 그대로 된다.
func (m *model) guard(p pendingRun) tea.Cmd {
	// 잠긴 prod 쓰기는 확인 창을 띄우지 않고 거절한다(R54). 열려 있으면 아래 확인은 그대로다
	if m.writeLocked(&p) {
		return nil
	}
	a := m.askOf(&p)
	if a == askNone {
		return m.start(p)
	}
	m.confirmSeq++
	p.ask = m.confirmSeq
	m.overlay = oConfirm
	m.pending = &p
	m.previewTop = 0
	if a != askWrite || p.kind != rkGrid || p.confirm != "" || p.stmts != 1 || m.conn(p.conn).Kind != db.KindPG {
		return nil
	}
	if w := db.FirstSQLWord(p.sql); w != "update" && w != "delete" {
		return nil
	}
	id, c, clients, life, sql := p.ask, m.conn(p.conn), m.clients, m.life, p.sql
	// p.sql 은 기록에 남길 확장 전 글이라 추정에는 enc() 를 바꾼 글을 보낸다(R52). 바꿀 수 없으면 run 이 이미 멈췄다
	if sent, _, err := m.expandEnc(p.conn, sql); err == nil {
		sql = sent
	}
	return func() tea.Msg {
		ctx, cancel := context.WithTimeout(life, estimateTimeout)
		defer cancel()
		pg, err := clients.PG(ctx, c)
		var n int64
		if err == nil {
			n, err = pg.EstimateRows(ctx, sql)
		}
		return estimateMsg{id: id, rows: n, err: err}
	}
}

func (m *model) start(p pendingRun) tea.Cmd {
	t := m.findTab(p.tab)
	if t == nil {
		m.status = "탭이 닫혀 실행하지 않음"
		return nil
	}
	// 확인 창을 띄운 사이 잠금 시간이 다 됐으면 y 를 눌러도 쓰지 않는다(R54)
	if m.writeLocked(&p) {
		return nil
	}
	var cat tea.Cmd
	if p.retarget && t.conn != p.conn {
		// 이 탭만 쓰던 접속의 트랜잭션을 접속을 바꾸며 몰래 버리지 않는다(R17.3). 끝낸 뒤 이 실행을 다시 시작한다
		if conns := m.orphanTx(t); len(conns) > 0 {
			return m.askTx(txPrompt{tab: t.id, conns: conns, after: afterRun, run: &p})
		}
		cat = m.setConn(t, p.conn)
	}
	if p.tv != nil {
		t.tv, t.title, t.viewConn, t.viewDB = p.tv, p.title, p.conn, m.conn(p.conn).DB
		ref := p.tv.Table()
		m.sess.TouchTable(m.conn(p.conn).UUID, ref.Schema, ref.Table, time.Now())
	}
	prevWrite := t.running != nil && !t.runningReadOnly
	if t.running != nil {
		t.running()
	}
	if p.readOnly && (p.kind == rkGrid || p.kind == rkPlan) {
		last := p
		t.last = &last
	}
	exec := p.exec
	// Manual 모드는 편집기 세션 실행 앞에 BEGIN 을 보낸다. 이미 열려 있으면 서버가 경고만 하고 넘어간다
	if p.tx && m.manual[p.conn] && !m.inTx[p.conn] {
		clients, c, inner := m.clients, m.conn(p.conn), p.exec
		exec = func(ctx context.Context) (*db.Result, error) {
			pg, err := clients.PG(ctx, c)
			if err != nil {
				return nil, err
			}
			if res, err := pg.Run(ctx, "BEGIN"); err != nil {
				return res, fmt.Errorf("begin: %w", err)
			}
			return inner(ctx)
		}
	}
	ctx, cancel := context.WithCancel(m.life)
	t.running, t.runningReadOnly, t.cancelled = cancel, p.readOnly, false
	m.seq++
	t.seq = m.seq
	seq, id, rev, began := m.seq, t.id, t.grid.Revision(), time.Now()
	session := p.tx || p.kind == rkTx
	m.status = m.label("실행 중… (Ctrl+C 취소)")
	if prevWrite {
		m.status = "이전 쓰기를 취소 요청함(이미 반영됐을 수 있음) · " + m.status
	}
	if p.note != "" {
		m.status = p.note + " · " + m.status
	}
	return tea.Batch(cat, func() tea.Msg {
		defer cancel()
		res, err := exec(ctx)
		msg := resultMsg{tab: id, seq: seq, conn: p.conn, sql: p.sql, stmts: p.stmts, res: res, err: err, kind: p.kind, key: p.key, old: p.old,
			session: session, rev: rev, start: began, changes: p.changes, undo: p.undo, params: p.params}
		if p.all != nil {
			msg.all = *p.all
		}
		return msg
	})
}

// gridKey 는 결과 표에 키를 넘기고, 표가 요청한 복사·내보내기를 명령으로 돌려준다.
// 표는 터미널·파일에 직접 쓰지 않으므로 OSC 52 와 파일 쓰기는 여기서 tea.Cmd 로 한다.
func (m *model) gridKey(k tea.KeyMsg) tea.Cmd {
	t := m.cur()
	var a grid.Action
	t.grid, a = t.grid.Update(k)
	if a.Status != "" {
		m.status = a.Status
	}
	switch {
	case a.Compare:
		// ─── 결과 비교(R31.2) ───
		// 고정한 결과 하나를 골라 지금 보이는 결과와 비교한다. 하나뿐이면 바로 비교한다.
		// 고정 이름(고정 N)은 풀고 다시 고정하면 겹칠 수 있어 항목 앞에 결과 탭 번호를 붙여 고른다
		var labels []string
		pins := map[string]*db.Result{}
		for i, s := range t.subs {
			if s.pinned && s.res != t.shown() {
				label := fmt.Sprintf("%d. %s", i+1, s.title)
				labels = append(labels, label)
				pins[label] = s.res
			}
		}
		switch len(labels) {
		case 0:
			m.status = "비교할 고정 결과가 없음 (p 로 고정)"
		case 1:
			if err := t.grid.ShowDiff(pins[labels[0]]); err != nil {
				m.status = err.Error()
			}
		default:
			m.openPicker("비교할 결과 ", labels, nil, func(label string) tea.Cmd {
				if err := m.cur().grid.ShowDiff(pins[label]); err != nil {
					m.status = err.Error()
				}
				return nil
			}, nil)
		}
		return nil
	case a.Follow != nil:
		// ─── 외래 키 따라가기 (R36) ───
		// 결과의 원본 테이블 FK 를 읽어 fkMsg 에서 참조 테이블(F)이나 참조하는 테이블 목록(R)을 연다.
		res := t.shown()
		if m.staleView(t) {
			return nil
		}
		if t.conn < 0 || m.conn(t.conn).Kind != db.KindPG || res == nil || res.Origin == nil {
			m.status = "원본 테이블을 모르는 결과"
			return nil
		}
		f, ref, conn, dbName := *a.Follow, res.Origin.Table, t.conn, m.conn(t.conn).DB
		m.status = "외래 키 찾는 중…"
		return withPG(m.life, m.clients, m.conn(conn), func(ctx context.Context, pg *db.PGConn, err error) tea.Msg {
			if err != nil {
				return fkMsg{conn: conn, db: dbName, follow: f, err: err}
			}
			var fks []db.FK
			if f.Reverse {
				fks, err = pg.ReferencedBy(ctx, ref)
			} else {
				fks, err = pg.ForeignKeys(ctx, ref)
			}
			return fkMsg{conn: conn, db: dbName, follow: f, fks: fks, err: err}
		})
	case a.Filter != nil:
		// ─── 칸 값으로 거르기 (R38) ───
		// 테이블 보기의 WHERE 에 칸 조건을 더하거나 이미 있으면 빼고 w 처럼 다시 읽는다.
		if t.tv == nil {
			m.status = "테이블 보기에서만 칸으로 거르기"
			return nil
		}
		f := *a.Filter
		tv, ta := t.tv.ToggleWhere(tableview.Condition(f.Col, f.Value, f.Null, f.Negate))
		return m.tableAction(tv, ta)
	case a.Decrypt:
		// ─── 복호화 보기 (R37) ───
		// 결과를 만든 접속 그룹의 .env 키로 staticEncrypt 칸을 풀어 보인다. 키는 상태 줄·기록 어디에도 내지 않는다.
		if t.grid.Decrypted() {
			t.grid.ClearDecrypted()
			m.status = "원래 값 보기"
			return nil
		}
		conn := t.viewConn
		if conn < 0 {
			conn = t.conn
		}
		if conn < 0 || m.conn(conn).Group == "" {
			m.status = "접속 그룹을 몰라 복호화 키를 고를 수 없음"
			return nil
		}
		name := db.EnvName(m.conn(conn).Group) + "_AES_KEY"
		key, _ := m.proj.AESKeyIV(m.conn(conn))
		if key == "" {
			m.status = name + " 가 .env 에 없음"
			return nil
		}
		failed := t.grid.SetDecrypted(statcrypt.LooksEncrypted, func(c string) (string, error) { return statcrypt.Decrypt(c, key) })
		m.status = "복호화 보기 (D 로 원래 값)"
		if failed > 0 {
			m.status = fmt.Sprintf("%d칸 풀지 못함", failed)
		}
		return nil
	case a.PickEnum != nil:
		m.pickEnum(t, *a.PickEnum)
		return nil
	case a.EditLong != nil:
		m.openLong(t, *a.EditLong)
		return nil
	case a.Copy != nil:
		return copyCmd(*a.Copy)
	case a.CopyAs != nil:
		sql := db.InsertSQL(t.shown(), a.CopyAs.Rows)
		if a.CopyAs.Kind == "update" {
			sql = db.UpdateSQL(t.shown(), a.CopyAs.Rows)
		}
		m.status = a.CopyAs.Kind + " 문 복사함"
		return copyCmd(sql)
	case a.Submit != nil:
		// ─── 편집 제출 ───
		// 표 편집 묶음을 확인 뒤 탭 접속의 편집기 세션에서 한 트랜잭션(또는 SAVEPOINT)으로 반영한다.
		cs := *a.Submit
		if m.staleView(t) {
			return nil
		}
		if t.conn < 0 || m.conn(t.conn).Kind != db.KindPG {
			m.status = "편집 제출은 PostgreSQL 탭에서만 됨"
			return nil
		}
		lines := db.ChangesSQL(cs)
		ops := map[db.ChangeOp]int{}
		for _, ch := range cs.Changes {
			ops[ch.Op]++
		}
		// 기록에는 값(비밀번호 칸일 수 있다) 없이 무엇을 몇 건 냈는지만 남긴다
		summary := fmt.Sprintf("-- 제출 %d건 %s (UPDATE %d · INSERT %d · DELETE %d)", len(cs.Changes), cs.Table.Quoted(),
			ops[db.ChangeUpdate], ops[db.ChangeInsert], ops[db.ChangeDelete])
		c, clients := m.conn(t.conn), m.clients
		return m.guard(pendingRun{tab: t.id, conn: t.conn, kind: rkSubmit, tx: true, stmts: len(lines), changes: &cs,
			confirm: fmt.Sprintf("%d건 제출", len(cs.Changes)), preview: lines, sql: summary,
			exec: func(ctx context.Context) (*db.Result, error) {
				pg, err := clients.PG(ctx, c)
				if err != nil {
					return nil, err
				}
				return pg.ApplyChanges(ctx, cs)
			}})
	case a.Export != nil:
		ex := *a.Export
		name := exportName(t.title, ex.Format)
		return func() tea.Msg {
			f, err := exportFile(name)
			if err != nil {
				return statusMsg("내보내기 실패: " + err.Error())
			}
			_, werr := f.WriteString(ex.Content)
			if err := errors.Join(werr, f.Close()); err != nil {
				return statusMsg("내보내기 실패: " + err.Error())
			}
			return statusMsg(fmt.Sprintf("%d행 내보냄: %s", ex.Rows, f.Name()))
		}
	}
	return nil
}

// copyCmd 는 글을 OSC 52 로 터미널 클립보드에 보낸다. 렌더러와 섞이지 않게 명령 고루틴에서 쓴다.
func copyCmd(s string) tea.Cmd {
	return func() tea.Msg {
		if _, err := osc52.New(s).WriteTo(clipboardOut); err != nil {
			return editor.ClipboardErrMsg{Err: err}
		}
		return nil
	}
}

// openPicker 는 고르기 창(파일·데이터베이스·스키마·즐겨찾기)을 연다. do 가 nil 이면 queries 파일 열기다.
// conn 은 항목 옆에 보일 접속 uuid, del 은 즐겨찾기 지우기다.
func (m *model) openPicker(prompt string, items []string, conn map[string]string, do, del func(string) tea.Cmd) {
	m.pickerItems, m.pickerConn, m.pickDo, m.pickDel, m.pickerTables = items, conn, do, del, nil
	m.pickerLower = make([]string, len(items))
	for i, it := range items {
		m.pickerLower[i] = strings.ToLower(it)
	}
	m.pickerCursor = 0
	m.input.SetValue("")
	m.input.Prompt = prompt
	m.input.Focus()
	m.overlay = oPicker
}

// prompt 는 한 줄 입력을 받아 Enter 면 do 를 부른다.
func (m *model) prompt(label, value string, do func(string) tea.Cmd) {
	m.promptDo = do
	m.input.Prompt = label
	m.input.SetValue(value)
	m.input.CursorEnd()
	m.input.Focus()
	m.overlay = oPrompt
}

// overlayBusy 는 늦게 온 비동기 결과가 창(고르기·확인)을 열려 할 때 다른 창이 이미 열려 있는지다.
// 열려 있으면 결과를 버리고 알린다. 사용자가 보고 있는 창을 바꿔치기해 엉뚱한 y·Enter 를 받지 않게 한다.
func (m *model) overlayBusy() bool {
	if m.overlay == oNone {
		return false
	}
	m.status = "다른 창이 열려 있어 결과를 버림"
	return true
}

// confirm 은 y 면 do 를 부르는 확인 창을 띄운다(실행이 아닌 확인).
func (m *model) confirm(q string, do func() tea.Cmd) {
	m.yes = &yesNo{q: q, do: do}
	m.overlay = oConfirm
}

// dbSwitchBlock 은 접속 conn 의 데이터베이스를 지금 바꾸면 안 되는 이유다. 비면 바꿔도 된다(R27.1).
// 실행 중이면 Forget 이 세션을 기다리고 Manual 의 첫 문장은 아직 inTx 가 아니라 트랜잭션을 몰래 버릴 수 있다(R17).
// 보류 중인 편집은 새 DB 에 낼 수 없어 사라지므로 먼저 끝내게 한다(R15).
func (m *model) dbSwitchBlock(conn int) string {
	if m.switching {
		return "데이터베이스를 바꾸는 중"
	}
	for _, t := range m.tabs {
		switch {
		case t.running != nil && t.conn == conn:
			return "실행 중인 쿼리를 먼저 끝내세요"
		case t.viewConn == conn && t.grid.Pending() > 0:
			return fmt.Sprintf(m.label("%s 탭의 변경 %d건을 먼저 제출(Ctrl+S)하거나 되돌리세요(u)"), t.title, t.grid.Pending())
		}
	}
	return ""
}

// switchDB 는 접속 conn 을 데이터베이스 name 으로 다시 연다(R27.1). 그 접속의 전체 내보내기는 취소해 Forget 이 풀 연결을 기다리지 않게 하고,
// 바꾼 DB 는 메모리의 Conn 에만 둔다. 열린 트랜잭션은 부르기 전에 끝냈다.
func (m *model) switchDB(conn int, name string) tea.Cmd {
	// 트랜잭션 확인을 기다리는 사이 시작한 실행·편집이 있을 수 있어 다시 본다
	if why := m.dbSwitchBlock(conn); why != "" {
		m.status = why
		return nil
	}
	if m.export != nil && m.export.conn == conn {
		m.export.canceled = true
		m.export.cancel()
	}
	m.switching = true
	prev := m.proj.Conns[conn].DB
	// Forget 이 끝나기 전에 시작한 조회도 새 DB 로 열도록 먼저 바꾼다. 실패하면 dbSwitchMsg 가 되돌린다
	m.proj.Conns[conn].DB = name
	m.status = "데이터베이스 바꾸는 중…"
	clients, uuid, life := m.clients, m.conn(conn).UUID, m.life
	return func() tea.Msg {
		ctx, cancel := context.WithTimeout(life, loadTimeout)
		defer cancel()
		return dbSwitchMsg{conn: conn, db: name, prev: prev, err: clients.Forget(ctx, uuid)}
	}
}

// describe 는 테이블 구조를 탭 t 의 결과로 읽는다. 트리 d 와 정의로 이동(R26)이 같이 쓴다.
func (m *model) describe(t *queryTab, n *node) tea.Cmd {
	if n.conn != t.conn && t.bound() {
		t = m.newTab()
	}
	ref, c, clients := db.TableRef{Schema: n.schema, Table: n.label}, m.conn(n.conn), m.clients
	return m.guard(pendingRun{tab: t.id, conn: n.conn, readOnly: true, retarget: true, exec: func(ctx context.Context) (*db.Result, error) {
		pg, err := clients.PG(ctx, c)
		if err != nil {
			return nil, err
		}
		res, err := pg.DescribeTable(ctx, ref)
		if err != nil {
			return nil, err
		}
		// 표 위 설명 줄은 한 줄만 보이므로 제약·인덱스를 한 줄로 이어 붙인다
		lines := strings.Split(res.Text, "\n")
		for i := range lines {
			lines[i] = strings.TrimSpace(lines[i])
		}
		res.Text = strings.Join(lines, " · ")
		return res, nil
	}})
}

// exportFile 은 내보내기 파일을 ~/Downloads(없으면 임시 폴더)에 새 파일·0600 으로 만든다(R14.2·R33.1).
// 결과 표 내보내기(e·E)와 전체 내보내기(F7)가 같이 쓴다. 명령 고루틴에서 부른다.
func exportFile(name string) (*os.File, error) {
	dir := os.TempDir()
	// 홈을 모르거나 Downloads 가 없으면 임시 폴더로 가면 되므로 오류는 버린다
	if home, err := os.UserHomeDir(); err == nil {
		if st, err := os.Stat(filepath.Join(home, "Downloads")); err == nil && st.IsDir() {
			dir = filepath.Join(home, "Downloads")
		}
	}
	// 임시 폴더의 남이 만든 파일·링크를 따라 쓰지 않게 새 파일로만 만든다
	return os.OpenFile(filepath.Join(dir, name), os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
}

// exportName 은 내보내기 파일 이름 db-<탭>-<YYYYMMDD-HHMMSS>.<ext> 다.
func exportName(title, ext string) string {
	name := strings.Trim(fileNameRe.ReplaceAllString(title, "-"), "-")
	if name == "" {
		name = "tab"
	}
	return fmt.Sprintf("db-%s-%s.%s", name, time.Now().Format("20060102-150405"), ext)
}

// notifyCmd 는 긴 실행이 끝났으면 OSC 9 알림을 보낸다(R30). SQL·값은 넣지 않는다.
func notifyCmd(conn string, elapsed time.Duration, rows int, failed, canceled bool) tea.Cmd {
	if !notify.ShouldNotify(elapsed, canceled, stdoutTTY()) {
		return nil
	}
	return func() tea.Msg {
		// 알림은 덤이라 쓰지 못해도 알리지 않는다
		_ = notify.Done(clipboardOut, conn, elapsed, rows, failed)
		return nil
	}
}

// ─── Update ───

func (m *model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.w, m.h = msg.Width, msg.Height
		m.resize()
		return m, nil
	case childrenMsg:
		if msg.gen != msg.n.gen {
			return m, nil
		}
		if msg.err != nil {
			label := "오류: " + m.errText(msg.n.conn, msg.err)
			msg.n.children = []*node{{kind: nInfo, label: label, conn: msg.n.conn, parent: msg.n}}
			m.jump, m.revealTab = nil, 0 // 읽기에 실패했으니 찾기를 이어 가지 않는다(다시 읽기를 되풀이하지 않게)
		} else {
			msg.n.children = msg.children
			msg.n.loaded = true
		}
		// 접속을 못 읽어도 그 접속의 쿼리 파일은 열 수 있게 붙인다(R43)
		if msg.n.kind == nConn {
			m.attachQueries(msg.n)
		}
		m.rebuild()
		if m.jump != nil {
			return m, m.reveal(m.jumpConn, *m.jump)
		}
		return m, nil
	case catalogMsg:
		if msg.gen != m.catGen[msg.conn] {
			return m, nil
		}
		if msg.err != nil {
			m.status = "자동완성 목록 실패: " + m.errText(msg.conn, msg.err)
			return m, nil
		}
		m.catalogs[msg.conn] = msg.cat
		return m, nil
	case ddlMsg:
		if msg.err != nil {
			m.status = "DDL 실패: " + m.errText(msg.conn, msg.err)
			return m, nil
		}
		t := m.newTab()
		cmd := m.setConn(t, msg.conn)
		t.title = "DDL " + msg.table
		t.editor.SetValue(msg.ddl)
		m.setFocus(fEditor)
		m.status = "DDL: " + msg.table
		return m, cmd
	case statusMsg:
		m.status = string(msg)
		return m, nil
	case connTestMsg:
		return m, m.connTested(msg)
	case defMsg:
		// ─── 정의로 이동 결과 (R26) ───
		// 테이블·뷰는 트리에서 고르고 구조를 열고, 함수는 정의를 텍스트 결과로 연다.
		if msg.err != nil {
			if errors.Is(msg.err, db.ErrNotFound) {
				m.status = "정의를 찾지 못함: " + msg.name
			} else {
				m.status = "정의 찾기 실패: " + m.errText(msg.conn, msg.err)
			}
			return m, nil
		}
		t := m.findTab(msg.tab)
		if t == nil {
			return m, nil
		}
		obj, c, clients := msg.obj, m.conn(msg.conn), m.clients
		if obj.Kind == db.ObjFunction {
			// 실행 전 확인(보류 중인 편집 버림·트랜잭션 끝내기)이 창을 열 수 있다
			if m.overlayBusy() {
				return m, nil
			}
			return m, m.guard(pendingRun{tab: t.id, conn: msg.conn, readOnly: true, retarget: true, exec: func(ctx context.Context) (*db.Result, error) {
				pg, err := clients.PG(ctx, c)
				if err != nil {
					return nil, err
				}
				def, err := pg.FunctionDef(ctx, obj.Schema, obj.Name)
				if err != nil {
					return nil, err
				}
				return &db.Result{Text: def}, nil
			}})
		}
		m.revealTab = t.id
		return m, m.reveal(msg.conn, finder.Item{Conn: c.Name, Kind: finder.KindTable, Schema: obj.Schema, Name: obj.Name})
	case pickListMsg:
		if msg.err != nil {
			m.status = "목록 읽기 실패: " + m.errText(msg.conn, msg.err)
			return m, nil
		}
		if len(msg.items) == 0 {
			m.status = "고를 항목이 없음"
			return m, nil
		}
		if m.overlayBusy() {
			return m, nil
		}
		m.openPicker(msg.prompt, msg.items, nil, msg.do, nil)
		return m, nil
	case dbSwitchMsg:
		m.switching = false
		// 전환 중 새 DB 이름으로 읽기 시작한 자동완성 목록은 성공·실패 어느 쪽이든 믿을 수 없어 버린다
		delete(m.catalogs, msg.conn)
		m.catGen[msg.conn]++
		delete(m.enums, msg.conn) // enum 은 데이터베이스마다 다르다(R49.2)
		if msg.err != nil {
			m.proj.Conns[msg.conn].DB = msg.prev
			m.status = "데이터베이스를 바꾸지 못함: " + m.errText(msg.conn, msg.err)
			return m, nil
		}
		// 옛 DB 에서 만든 내보내기 문장을 새 DB 에 보내지 않는다. 결과·테이블 보기는 viewDB 로 staleView 가 막는다
		for _, t := range m.tabs {
			if t.exportConn == msg.conn {
				t.exportSQL = ""
			}
		}
		// 새 세션은 트랜잭션 밖이고 고른 search_path 도 옛 DB 의 것이다
		if m.dbOver[msg.conn] == "" {
			m.dbOrig[msg.conn] = msg.prev
		}
		m.dbOver[msg.conn] = msg.db
		m.inTx[msg.conn] = false
		delete(m.schemaOf, msg.conn)
		m.status = fmt.Sprintf("데이터베이스 바꿈: %s/%s", m.conn(msg.conn).Name, msg.db)
		var reload tea.Cmd
		if cn := m.connNode(msg.conn); cn != nil {
			cn.loaded = false
			reload = m.expand(cn)
		}
		return m, tea.Batch(reload, m.ensureCatalog(msg.conn))
	case searchPathMsg:
		if msg.err != nil {
			m.status = "search_path 실패: " + m.errText(msg.conn, msg.err)
			return m, nil
		}
		m.schemaOf[msg.conn] = msg.schema
		m.status = fmt.Sprintf("search_path: %s, public", msg.schema)
		return m, nil
	case searchCountMsg:
		if msg.err != nil {
			m.status = "검색할 테이블 수를 세지 못함: " + m.errText(msg.conn, msg.err)
			return m, nil
		}
		if m.overlayBusy() {
			return m, nil
		}
		m.confirm(fmt.Sprintf("prod(%s) 테이블 %d개 전체 검색", m.conn(msg.conn).Name, msg.n), msg.open)
		return m, nil
	case exportTickMsg:
		if m.export != msg.job {
			return m, nil
		}
		m.status = fmt.Sprintf("전체 내보내는 중… %s바이트 (esc 취소)", textutil.GroupDigits(msg.job.bytes.Load()))
		return m, tea.Tick(exportTick, func(time.Time) tea.Msg { return exportTickMsg{job: msg.job} })
	case exportDoneMsg:
		if m.export == msg.job {
			m.export = nil
		}
		c := m.conn(msg.job.conn)
		canceled := msg.job.canceled
		switch {
		case canceled && msg.err != nil:
			m.status = "전체 내보내기 취소함"
		case msg.err != nil:
			m.status = "전체 내보내기 실패: " + m.errText(msg.job.conn, msg.err)
		default:
			m.status = fmt.Sprintf("%s행 전체 내보냄: %s", textutil.GroupDigits(msg.rows), msg.path)
		}
		return m, notifyCmd(c.Name, time.Since(msg.job.start), int(msg.rows), msg.err != nil, canceled)
	case editor.ClipboardErrMsg:
		m.status = "클립보드 복사 실패: " + msg.Err.Error()
		return m, nil
	case fkMsg:
		// ─── 외래 키 따라가기 결과 (R36) ───
		// F 는 커서 열이 든 첫 FK(이름순)의 참조 테이블을, R 은 고른 참조 테이블을 새 탭의 테이블 보기로 연다.
		if m.conn(msg.conn).DB != msg.db {
			m.status = "외래 키를 읽는 사이 데이터베이스를 바꿔 열지 않음"
			return m, nil
		}
		if msg.err != nil {
			m.status = "외래 키 읽기 실패: " + m.errText(msg.conn, msg.err)
			return m, nil
		}
		if m.overlayBusy() {
			return m, nil
		}
		f, conn := msg.follow, msg.conn
		if !f.Reverse {
			for _, fk := range msg.fks {
				if !slices.Contains(fk.Cols, f.Col) {
					continue
				}
				where, why := fkWhere(f.Row, fk.Cols, fk.RefCols, fk.Name)
				if why != "" {
					m.status = why
					return m, nil
				}
				return m, m.openTable(m.newTab(), conn, fk.Ref, where)
			}
			m.status = "외래 키 열이 아님"
			return m, nil
		}
		if len(msg.fks) == 0 {
			m.status = "이 테이블을 참조하는 테이블이 없음"
			return m, nil
		}
		labels, byLabel := make([]string, 0, len(msg.fks)), map[string]db.FK{}
		for _, fk := range msg.fks {
			label := fmt.Sprintf("%s.%s (%s)", fk.Table.Schema, fk.Table.Table, strings.Join(fk.Cols, ", "))
			if _, dup := byLabel[label]; dup {
				label += " " + fk.Name
			}
			labels, byLabel[label] = append(labels, label), fk
		}
		m.openPicker("참조하는 테이블 ", labels, nil, func(label string) tea.Cmd {
			fk := byLabel[label]
			where, why := fkWhere(f.Row, fk.RefCols, fk.Cols, fk.Name)
			if why != "" {
				m.status = why
				return nil
			}
			return m.openTable(m.newTab(), conn, fk.Table, where)
		}, nil)
		return m, nil
	case enumMsg:
		// 늦게 온 이전 DB 의 답이나 실패는 버린다. 실패한 타입은 다시 읽도록 읽는 중 표시를 지운다
		if m.conn(msg.conn).DB != msg.db || msg.err != nil {
			for typ, labels := range m.enums[msg.conn] {
				if labels == nil {
					delete(m.enums[msg.conn], typ)
				}
			}
			if msg.err != nil {
				m.status = "enum 값 읽기 실패: " + m.errText(msg.conn, msg.err)
			}
			return m, nil
		}
		if m.enums[msg.conn] == nil {
			m.enums[msg.conn] = map[string][]string{}
		}
		for typ, labels := range msg.labels {
			// enum 이 아닌 타입도 빈 목록으로 남겨 다시 묻지 않는다
			m.enums[msg.conn][typ] = append([]string{}, labels...)
		}
		set := m.enumSet(msg.conn)
		for _, t := range m.tabs {
			if t.viewConn == msg.conn {
				t.grid.SetEnumTypes(set)
			}
		}
		return m, nil
	case multiMsg:
		m.showMulti(msg)
		return m, nil
	case schemaDiffMsg:
		m.showSchemaDiff(msg)
		return m, nil
	case lockTickMsg:
		m.lockTicking = false
		m.expireLocks()
		return m, m.lockTicker()
	case estimateMsg:
		// 늦게 온 이전 확인 창의 추정은 지금 확인 창에 붙이지 않는다. 실패·시간 초과면 빼고 보인다(R39)
		if msg.err == nil && m.overlay == oConfirm && m.pending != nil && m.pending.ask == msg.id {
			m.pending.estimate = fmt.Sprintf("약 %s행 (추정)", textutil.GroupDigits(msg.rows))
		}
		return m, nil
	case keyScanMsg:
		// ─── Redis 키 일괄 삭제 미리 보기 (R42) ───
		// 개수와 앞 키를 보이고 y 면 SCAN 으로 센 키만 배치로 UNLINK 한다. 늘 묻는다.
		c := m.conn(msg.conn)
		switch {
		case msg.err != nil:
			m.status = "키 찾기 실패: " + m.errText(msg.conn, msg.err)
			return m, nil
		case msg.truncated:
			m.status = fmt.Sprintf("키가 %s개를 넘어 지우지 않음 (패턴을 좁히세요)", textutil.GroupDigits(redisDeleteLimit))
			return m, nil
		case len(msg.keys) == 0:
			m.status = "맞는 키가 없음: " + msg.pattern
			return m, nil
		case m.overlayBusy():
			return m, nil
		}
		name := c.Name
		if c.Env() == "prod" {
			name = "prod(" + name + ")"
		}
		q := fmt.Sprintf("%s에서 키 %s개 지움", name, textutil.GroupDigits(int64(len(msg.keys))))
		if matchesAll(msg.pattern) {
			q += " · 위험: FLUSHDB 와 같음"
		}
		preview := []string{fmt.Sprintf("패턴 %s · 키 %s개 · 앞 %d개:", msg.pattern, textutil.GroupDigits(int64(len(msg.keys))), min(len(msg.keys), redisDeletePreview))}
		preview = append(preview, msg.keys[:min(len(msg.keys), redisDeletePreview)]...)
		keys, conn, clients, life := msg.keys, msg.conn, m.clients, m.life
		m.confirm(q, func() tea.Cmd {
			if m.unlink != nil {
				m.status = "키를 지우는 중 (esc 멈춤)"
				return nil
			}
			if m.lockRefuse(conn) {
				return nil
			}
			ctx, cancel := context.WithCancel(life)
			job := &unlinkJob{cancel: cancel, total: len(keys), conn: conn}
			m.unlink = job
			m.status = fmt.Sprintf("키 지우는 중… 0/%s (esc 멈춤)", textutil.GroupDigits(int64(len(keys))))
			return tea.Batch(tea.Tick(exportTick, func(time.Time) tea.Msg { return unlinkTickMsg{job: job} }), func() tea.Msg {
				defer cancel()
				rc, err := clients.Redis(ctx, c)
				n := 0
				if err == nil {
					n, err = rc.UnlinkKeys(ctx, keys, redisDeleteBatch, func(done int) { job.done.Store(int64(done)) })
				}
				return unlinkDoneMsg{job: job, n: n, err: err}
			})
		})
		m.yes.preview = preview
		return m, nil
	case unlinkTickMsg:
		if m.unlink != msg.job {
			return m, nil
		}
		m.status = fmt.Sprintf("키 지우는 중… %s/%s (esc 멈춤)", textutil.GroupDigits(msg.job.done.Load()), textutil.GroupDigits(int64(msg.job.total)))
		return m, tea.Tick(exportTick, func(time.Time) tea.Msg { return unlinkTickMsg{job: msg.job} })
	case unlinkDoneMsg:
		if m.unlink == msg.job {
			m.unlink = nil
		}
		switch {
		case msg.job.canceled:
			m.status = fmt.Sprintf("키 지우기 멈춤: %s개 지움", textutil.GroupDigits(int64(msg.n)))
		case msg.err != nil:
			m.status = fmt.Sprintf("키 지우기 실패 (%s개 지움): %s", textutil.GroupDigits(int64(msg.n)), m.errText(msg.job.conn, msg.err))
		default:
			m.status = fmt.Sprintf("%s개 지움", textutil.GroupDigits(int64(msg.n)))
		}
		cn := m.connNode(msg.job.conn)
		if cn == nil {
			return m, nil
		}
		cn.loaded = false
		return m, m.expand(cn)
	case resultMsg:
		// ─── 실행 결과 ───
		// 실행 하나의 결과를 기록하고 종류에 맞게 탭에 붙인다.
		c := m.conn(msg.conn)
		e := history.Entry{Time: time.Now(), Conn: c.Name, SQL: db.Redact(db.MaskSecrets(msg.sql), c.Password)}
		if msg.err != nil {
			e.Err = db.Redact(msg.err.Error(), c.Password)
		}
		if msg.res != nil {
			e.Elapsed, e.Rows = msg.res.Elapsed, len(msg.res.Rows)
		}
		var save tea.Cmd
		if msg.sql != "" && m.hist != nil {
			hist := m.hist
			save = func() tea.Msg {
				if err := hist.Append(e); err != nil {
					return statusMsg("기록 실패: " + err.Error())
				}
				return nil
			}
		}
		t := m.findTab(msg.tab)
		// 다시 실행·탭 닫기로 밀려난 결과는 화면에 안 붙이지만, 취소 전에 반영됐을 수 있는 쓰기라 기록은 남긴다
		if t == nil || msg.seq != t.seq {
			return m, save
		}
		t.running = nil
		if msg.err == nil && !t.cancelled && len(msg.params) > 0 {
			save = tea.Batch(save, m.saveParams(msg.params))
		}
		if msg.kind != rkEstimate && !msg.start.IsZero() {
			rows := 0
			if msg.res != nil {
				rows = len(msg.res.Rows)
			}
			// 아래의 모든 반환이 save 를 싣고 나가므로 알림을 save 에 붙인다
			save = tea.Batch(save, notifyCmd(c.Name, time.Since(msg.start), rows, msg.err != nil, t.cancelled))
		}
		var tick tea.Cmd
		if msg.res != nil && msg.session {
			// 트랜잭션이 막 열렸으면 헤더 시계의 시작을 적는다
			if msg.res.InTx && !m.inTx[msg.conn] {
				m.txSince[msg.conn] = time.Now()
			}
			m.inTx[msg.conn] = msg.res.InTx
			tick = m.tick()
		}
		// 표를 바꾸는 결과(편집기 실행·테이블 보기·실행 계획)는 탭의 결과로 붙인다
		if replacesGrid[msg.kind] {
			// 실행하는 사이 고친 편집은 버릴지 물은 적이 없으므로 결과로 덮지 않는다
			if n := t.grid.Pending(); n > 0 && t.grid.Revision() != msg.rev {
				m.status = fmt.Sprintf("실행 중에 고친 변경 %d건이 있어 결과를 붙이지 않음 (제출하거나 되돌린 뒤 다시 실행)", n)
				return m, tea.Batch(save, tick)
			}
			res := msg.res
			if msg.err != nil {
				res = nil
			}
			if msg.kind == rkEstimate {
				// 추정치를 못 읽어도(권한 등) 페이지는 읽는다. 개수만 모르는 채로 둔다
				if res != nil && len(res.Rows) == 1 && t.tv != nil {
					if n, err := strconv.ParseInt(res.Rows[0][0], 10, 64); err == nil {
						*t.tv = t.tv.SetTotal(n, false)
					}
				}
				if t.tv == nil {
					return m, tea.Batch(save, tick)
				}
				return m, tea.Batch(save, tick, m.start(m.tablePage(t)))
			}
			t.plan, t.planOn, t.redis = nil, false, nil
			if msg.kind == rkPlan && res != nil && len(res.Rows) == 1 && len(res.Cols) == 1 {
				if root, err := plantree.Parse(res.Rows[0][0]); err == nil {
					pm := plantree.New(root)
					t.plan, t.planOn = &pm, true
					// t 로 돌아가 보는 글 화면은 JSON 원문이다. 실행 계획을 TEXT 로 다시 받으면 ANALYZE 가 문장을 한 번 더 실행한다
					res = &db.Result{Text: db.PrettyJSON(res.Rows[0][0]), InTx: res.InTx, Elapsed: res.Elapsed}
				}
			}
			t.viewConn, t.viewDB = msg.conn, c.DB
			// 전체 내보내기(R33.1)는 지금 보이는 표를 만든 문장만 다시 보낸다. 실패·취소한 문장이나 계획은 남기지 않는다
			t.exportSQL, t.exportConn = "", msg.conn
			if msg.kind == rkGrid && msg.err == nil && !t.cancelled {
				t.exportSQL = msg.sql
			}
			if msg.kind != rkTable {
				t.tv = nil
			} else if res != nil && t.tv != nil {
				*t.tv = t.tv.SetColumns(res.Cols, res.Types)
				if off, size := t.tv.Page(); len(res.Rows) < size {
					*t.tv = t.tv.SetTotal(int64(off+len(res.Rows)), true)
				}
			}
			t.res, t.resErr = res, e.Err
			pins := t.subs[:0:0]
			for _, s := range t.subs {
				if s.pinned {
					pins = append(pins, s)
				}
			}
			t.subs, t.view = pins, -1
			if res != nil && len(msg.all) > 1 {
				for i, r := range msg.all {
					t.subs = append(t.subs, subResult{title: fmt.Sprintf("문장 %d", i+1), res: r})
				}
			}
			keepCol := t.grid.CurrentCol()
			t.grid.SetResult(t.res)
			// 테이블 보기는 정렬·페이지를 바꿔 다시 읽어도 커서 열을 지켜 s 를 거듭 누르면 같은 열이 돈다
			if msg.kind == rkTable {
				t.grid.FocusCol(keepCol)
			}
			// enum 열은 고르기 창으로 고친다(R49.2). 모르는 사용자 타입만 읽는다
			tick = tea.Batch(tick, m.loadEnums(t, msg.conn, t.res))
			switch {
			case t.cancelled:
				// 늦게 온 결과가 취소 경고를 덮지 않게 한다(R3.4)
				t.summary = "취소됨"
				if !t.runningReadOnly {
					t.summary += " (쓰기는 이미 반영됐을 수 있음)"
				}
			case msg.err != nil:
				t.summary = "실패"
			case len(msg.res.Cols) > 0 || t.plan != nil:
				t.summary = fmt.Sprintf("%d rows · %s", len(msg.res.Rows), msg.res.Elapsed.Round(time.Millisecond))
				if msg.res.Truncated {
					t.summary += fmt.Sprintf(" · %d에서 자름", db.MaxRows)
				}
			default:
				t.summary = oneLine(msg.res.Text) + " · " + msg.res.Elapsed.Round(time.Millisecond).String()
			}
			if msg.stmts > 1 {
				t.summary = fmt.Sprintf("%d statements · %s", msg.stmts, t.summary)
			}
			if msg.res != nil && msg.res.InTx {
				t.summary += " · 트랜잭션 열림"
			}
			m.status = t.summary
			if t != m.cur() {
				m.status = t.title + ": " + t.summary
			}
			return m, tea.Batch(save, tick)
		}
		var cmd tea.Cmd
		status := ""
		switch {
		case t.cancelled:
			status = "취소됨"
		case msg.err != nil:
			status = "실패: " + oneLine(e.Err)
		}
		switch msg.kind {
		case rkCount:
			if status == "" && len(msg.res.Rows) == 1 && t.tv != nil {
				n, err := strconv.ParseInt(msg.res.Rows[0][0], 10, 64)
				if err != nil {
					status = "행 수를 읽지 못함: " + err.Error()
					break
				}
				*t.tv = t.tv.SetTotal(n, true)
				status = fmt.Sprintf("전체 %d행", n)
			}
		case rkSubmit:
			if status != "" && msg.undo != nil {
				// 옛값 확인이 실패하면(제출 뒤 다른 사람이 고침) 묶음 전체가 되돌려지고 되돌리기 묶음은 남는다(R50.2)
				status = "되돌리기 " + status + " · 묶음은 남김"
				break
			}
			if status != "" {
				// 실패하면 묶음 전체가 되돌려졌으므로 보류 중인 편집을 남겨 고쳐 다시 내게 한다
				status = "제출 " + status
				break
			}
			if msg.undo != nil {
				// 되돌리기는 보류 중인 편집과 상관없는 묶음이라 편집은 그대로 두고, 없을 때만 다시 읽는다
				t.undo = slices.DeleteFunc(t.undo, func(e *undoEntry) bool { return e == msg.undo })
				status = "되돌림: " + oneLine(msg.res.Text)
				if t.grid.Pending() == 0 {
					cmd = m.refresh(t)
				}
				break
			}
			status = "제출함: " + oneLine(msg.res.Text)
			if msg.changes != nil {
				note, log := m.pushUndo(t, msg.conn, *msg.changes, msg.res.Applied)
				if note != "" {
					status += " · " + note
				}
				cmd = log
			}
			// 낸 바꿈만 지우고 제출하는 사이 고친 것은 남긴다. 남은 게 있으면 다시 읽으면 사라지므로 표를 그대로 둔다.
			// 버린 행이 있을 때도 다시 읽지 않는다. 다시 읽은 결과의 요약이 버렸다는 알림을 덮기 때문이다
			dropped := t.grid.ClearSubmitted(msg.rev)
			if dropped > 0 {
				status += fmt.Sprintf(" · 제출 중 고친 추가·삭제 행 %d개는 버림: 다시 읽은 뒤 고치세요", dropped)
			}
			if n := t.grid.Pending(); n > 0 {
				status += fmt.Sprintf(" · 제출 중 고친 변경 %d건은 남김", n)
			}
			if dropped == 0 && t.grid.Pending() == 0 {
				cmd = tea.Batch(cmd, m.refresh(t))
			}
		case rkSignal:
			if status == "" {
				status = "보냄: " + oneLine(msg.res.Text)
				cmd = m.refresh(t)
			}
		case rkTx:
			switch {
			case status != "":
			case msg.sql == "COMMIT" && msg.res.Text != "COMMIT":
				status = commitRolledBack
			default:
				status = oneLine(msg.res.Text)
			}
		case rkRedisWrite:
			if status == "" {
				status, cmd = m.afterRedisWrite(t, msg)
			}
		default:
			if status == "" {
				status = oneLine(msg.res.Text)
			}
		}
		m.status = status
		if t != m.cur() {
			m.status = t.title + ": " + status
		}
		return m, tea.Batch(save, tick, cmd)
	case redisValueMsg:
		// ─── Redis 값 읽기 결과 ───
		t := m.findTab(msg.tab)
		if t == nil {
			return m, nil
		}
		if msg.err != nil {
			m.status = "키 읽기 실패: " + m.errText(msg.conn, msg.err)
			return m, nil
		}
		// db.RedisValue 는 redisview.Value 와 필드가 같게 맞춰 두었다(db 가 화면 패키지를 가져오지 않게 하려고 두 벌)
		ed := redisview.NewEditor(redisview.Value(*msg.v))
		// 편집 뒤 다시 읽은 것이면 실행 결과 알림을 덮지 않는다
		reload := t.redis != nil && t.redisKey == msg.v.Key
		t.redis, t.redisKey, t.viewConn, t.viewDB = &ed, msg.v.Key, msg.conn, m.conn(msg.conn).DB
		t.plan, t.planOn = nil, false
		switch {
		case msg.v.Truncated:
			m.status = "일부만 읽음(1MiB·1,000개 상한): 큰 값은 고치지 않는 편이 안전"
		case !reload:
			m.status = "값 편집: e 고치기 · r 이름 · t TTL · D 지우기 · esc 닫기"
		}
		return m, nil
	case txDoneMsg:
		// ─── 트랜잭션 끝 ───
		// 끝낸 접속의 트랜잭션 표시를 지우고, 모두 끝났을 때만 미뤄 둔 동작을 한다.
		m.txAsk = nil
		for _, c := range msg.done {
			m.inTx[c] = false
		}
		if msg.err != nil {
			text := msg.err.Error()
			for _, c := range msg.prompt.conns {
				text = db.Redact(text, m.conn(c).Password)
			}
			if errors.Is(msg.err, errCommitRolledBack) {
				text = commitRolledBack
			}
			m.status = "트랜잭션을 끝내지 못해 멈춤: " + oneLine(text)
			return m, nil
		}
		m.status = "트랜잭션을 끝냄"
		switch msg.prompt.after {
		case afterQuit:
			return m, m.quit()
		case afterDB:
			return m, m.switchDB(msg.prompt.conns[0], msg.prompt.db)
		}
		// 끝나기를 기다리는 사이 다른 탭으로 옮겼을 수 있어 확인을 띄운 탭에 동작한다
		t := m.findTab(msg.prompt.tab)
		if t == nil {
			return m, nil
		}
		switch msg.prompt.after {
		case afterClose:
			m.closeTab(t)
		case afterActivate:
			m.switchTab(slices.Index(m.tabs, t))
			return m, m.activate(msg.prompt.node, false)
		case afterRun:
			return m, m.start(*msg.prompt.run)
		}
		return m, nil
	case tickMsg:
		m.ticking = false
		return m, m.tick()
	case autoSaveMsg:
		if t := m.findTab(msg.tab); t != nil && t.editRev == msg.rev {
			m.autoSaveStatus(t)
		}
		return m, nil
	case paneSaveMsg:
		if msg.rev == m.paneRev {
			m.savePanes()
		}
		return m, nil
	case tea.MouseMsg:
		// 끄는 중에 창이 떴으면 놓기를 창이 먹으므로 여기서 끌기를 끝낸다
		if m.overlay != oNone {
			m.endDrag()
		}
		if m.overlay == oHelp && msg.Action == tea.MouseActionPress {
			m.overlay = oNone
			return m, nil
		}
		// 오버레이 중 클릭이 뒤 화면(확인 대기 중인 실행 등)을 건드리지 않게 막는다
		if m.overlay != oNone || m.w < minWidth || m.h < minHeight {
			return m, nil
		}
		l := m.layout()
		rightX := l.treeW + 2
		// ─── 경계 끌기 (R46.1) ───
		// 트리 오른쪽 테두리(treeW+1)와 오른쪽 영역 왼쪽 테두리(rightX), 편집기 아래 테두리(edH+3)와 결과 위 테두리(edH+4)가 경계다.
		// 누르면 잡고, 버튼을 누른 채 움직이면 크기를 바꾸고, 놓으면 세션에 저장한다(R46.2).
		// 버튼 없는 움직임이나 새 누르기가 오면 놓기를 놓친 것(창 밖에서 놓음 등)이라 끌기를 끝낸다
		if m.drag != bNone {
			if msg.Action == tea.MouseActionMotion && msg.Button != tea.MouseButtonNone {
				// 끌기 시작한 그려진 크기로 돌아오면 저장한 크기로 둔다. 작은 창에서 줄여 그린 크기로 덮지 않는다.
				// 한계: 작은 창에서 일부러 그려진 크기(dragBase)에 놓아도 저장한 큰 크기(dragFrom)로 남는다. 의도를 가를 수 없어서다.
				// 한 칸이라도 옮겨 놓으면 그 크기를 저장하므로 실사용 피해는 작다. 고치려면 놓을 때 움직인 거리로 갈라야 한다
				if m.drag == bTree {
					m.treeW = m.clampTree(msg.X - m.dragOff)
					if m.treeW == m.dragBase {
						m.treeW = m.dragFrom
					}
				} else {
					m.edH = m.clampEditor(max(1, msg.Y-m.dragOff)) // 0 은 기본 높이라 위로 넘겨 끌어도 하한으로 둔다
					if m.edH == m.dragBase {
						m.edH = m.dragFrom
					}
				}
				m.resize()
				return m, nil
			}
			m.endDrag()
			if msg.Action != tea.MouseActionPress {
				return m, nil
			}
		}
		if msg.Action == tea.MouseActionPress && msg.Button == tea.MouseButtonLeft {
			switch {
			case msg.Y < 1 || msg.Y > l.bodyH:
			case msg.X == l.treeW+1 || (msg.X == rightX && msg.Y >= 2): // 1줄은 탭 줄이라 rightX 에서 첫 탭이 시작한다
				m.drag, m.dragOff, m.dragFrom, m.dragBase = bTree, msg.X-l.treeW, m.treeW, l.treeW
				return m, nil
			case msg.X > rightX && (msg.Y == l.edH+3 || msg.Y == l.edH+4):
				m.drag, m.dragOff, m.dragFrom, m.dragBase = bSplit, msg.Y-l.edH, m.edH, l.edH
				return m, nil
			}
		}
		t := m.cur()
		// 편집기 내용은 오른쪽 테두리 안(rightX+1), 헤더·탭 줄·위 테두리 아래(3)에서 시작한다
		edMsg := msg
		edMsg.X, edMsg.Y = msg.X-rightX-1, msg.Y-3
		if msg.Action != tea.MouseActionPress {
			// 끌기·놓기는 편집기 선택만 쓴다. 영역 밖으로 끌어도 편집기가 이어 받는다
			if m.focus == fEditor {
				t.editor.HandleMouse(edMsg)
			}
			return m, nil
		}
		wheel := tea.MouseEvent(msg).IsWheel()
		if !wheel && msg.Button != tea.MouseButtonLeft {
			return m, nil
		}
		switch {
		case msg.Y < 1 || msg.Y > l.bodyH:
			// 헤더·상태 줄
		case msg.X < rightX && wheel:
			delta := 0
			switch msg.Button {
			case tea.MouseButtonWheelUp:
				delta = -wheelStep
			case tea.MouseButtonWheelDown:
				delta = wheelStep
			}
			m.treeTop = clamp(m.treeTop+delta, max(1, len(m.rows)-l.treeH+1))
			// 커서가 화면 밖에 남으면 View 가 커서 쪽으로 되돌려 휠이 먹지 않으므로 커서를 화면 안으로 끌어온다
			m.cursor = clamp(max(m.treeTop, min(m.cursor, m.treeTop+l.treeH-1)), len(m.rows))
		case msg.X < rightX:
			m.setFocus(fTree)
			i := m.treeTop + msg.Y - 2
			if msg.Y < 2 || msg.Y >= 2+l.treeH || i >= len(m.rows) {
				return m, nil
			}
			m.cursor = i
			r := m.rows[i]
			// 펼침 표시는 왼쪽 테두리(1칸) 다음, 깊이마다 2칸 들여 쓴 자리의 2칸이다
			onToggle := r.n.expandable() && msg.X >= 1+2*r.depth && msg.X < 3+2*r.depth
			double := m.lastClick.row == i && time.Since(m.lastClick.at) < doubleClick
			m.lastClick = click{row: i, at: time.Now()}
			if onToggle || double {
				m.lastClick = click{row: -1} // 세 번째 클릭이 다시 더블클릭으로 잡히지 않게 한다
				return m, m.activate(r.n, true)
			}
		case msg.Y == 1 && !wheel:
			x := rightX
			for i, lb := range m.tabLabels() {
				if msg.X >= x && msg.X < x+lipgloss.Width(lb) {
					m.switchTab(i)
					return m, nil
				}
				x += lipgloss.Width(lb) + 1
			}
			if msg.X >= x && msg.X < x+lipgloss.Width(tabPlus) {
				m.newTab()
				m.setFocus(fEditor)
			}
		case msg.Y == 1:
		case msg.Y < l.edH+4:
			if !wheel {
				m.setFocus(fEditor)
			}
			t.editor.HandleMouse(edMsg)
		default:
			if !wheel {
				m.setFocus(fResult)
			}
			// 결과 내용은 편집기 영역(edH+2)과 결과 위 테두리 아래에서 시작한다
			gm := msg
			gm.X, gm.Y = msg.X-rightX-1, msg.Y-l.edH-5-len(m.resultLines(t, l.rightW))
			t.grid.HandleMouse(gm)
			if t.redis != nil || t.planOn || wheel {
				return m, nil
			}
			// 테이블 보기 바로 위 줄은 WHERE·ORDER BY 칸이다. 누른 칸의 입력을 연다(R16.4)
			if t.tv != nil && gm.Y == -1 {
				if tv, a := t.tv.BarClick(gm.X, l.rightW); a.Handled {
					return m, m.tableAction(tv, a)
				}
			}
			// 열 머리 클릭은 s 와 같은 정렬이다. 테이블 보기는 서버 정렬(shift 는 정렬 열 더하기), 아니면 받은 행 정렬(R14.3)
			col, ok := t.grid.HeaderAt(gm.X, gm.Y)
			switch {
			case !ok:
			case t.tv != nil && msg.Shift:
				return m, m.tableAction(t.tv.HeaderClickAdd(col))
			case t.tv != nil:
				return m, m.tableAction(t.tv.HeaderClick(col))
			default:
				t.grid.CycleSort()
			}
		}
		return m, nil
	case tea.KeyMsg:
		// 쪼개져 들어온 SGR 마우스 보고(`[<64;84;23M`)는 bubbletea 가 글자로 넘긴다. 편집기·입력 줄에 들어가
		// 자동 저장으로 파일까지 망가뜨리므로 버린다. 붙여넣기 본문은 사용자가 넣은 글이라 건드리지 않는다
		if msg.Type == tea.KeyRunes && !msg.Paste && mouseLeakRe.MatchString(string(msg.Runes)) {
			return m, nil
		}
	default:
		// 커서 깜박임 같은 입력 줄 메시지는 입력 줄로 보낸다. 편집기는 키 말고는 받지 않는다
		var cmd tea.Cmd
		m.input, cmd = m.input.Update(msg)
		return m, cmd
	}

	k := msg.(tea.KeyMsg)
	ks := k.String()
	t := m.cur()

	// Ctrl+C 는 어떤 오버레이에서든 먼저 처리한다
	if ks == "ctrl+c" {
		if t.running != nil {
			t.running()
			t.running, t.cancelled = nil, true // 결과를 기다리지 않고 두 번째 Ctrl+C 로 끝낼 수 있게 한다
			// 쓰기는 서버에 이미 반영됐을 수 있어 취소를 확정하지 않는다(E18)
			m.status = "취소 요청함"
			if !t.runningReadOnly {
				m.status += " (쓰기는 이미 반영됐을 수 있음)"
			}
			return m, nil
		}
		// 트랜잭션 확인 창에서 한 번 더 누르면 강제로 끝낸다. 접속이 끊긴 채 COMMIT·ROLLBACK 이 실패해도 나갈 수 있어야 하고,
		// 끊긴 세션의 트랜잭션은 서버가 되돌린다
		if m.overlay == oTx {
			return m, m.quit()
		}
		return m, m.quitAsk()
	}

	// ─── 오버레이 ───
	switch m.overlay {
	case oLong:
		return m, m.longKey(k)
	case oConn:
		return m, m.connFormKey(k)
	case oHelp:
		m.overlay = oNone
		return m, nil
	case oConfirm:
		if y := m.yes; y != nil {
			m.overlay, m.yes = oNone, nil
			if ks != "y" {
				m.status = "취소함"
				return m, nil
			}
			return m, y.do()
		}
		p := m.pending
		if d, ok := previewKeys[ks]; ok && p != nil && len(p.preview) > 0 {
			m.previewTop = clamp(m.previewTop+d, len(p.preview))
			return m, nil
		}
		m.overlay = oNone
		m.pending = nil
		if ks != "y" || p == nil {
			if p != nil && p.restore != nil {
				if t := m.findTab(p.tab); t != nil && t.tv != nil {
					*t.tv = *p.restore
				}
			}
			m.status = "취소함"
			return m, nil
		}
		// 한 단계를 답하면 다음 단계(ANALYZE 뒤 prod 등)를 다시 묻는다
		switch m.askOf(p) {
		case askDiscard:
			p.discardOK = true
			return m, m.guard(*p)
		case askAnalyze:
			p.analyze = false
			return m, m.guard(*p)
		}
		return m, m.start(*p)
	case oParams:
		// ─── 파라미터 입력 줄 ───
		// Enter 는 다음 자리, 마지막이면 실행, esc 는 취소다.
		// 값은 문자열로 보내 서버가 자리의 타입으로 읽는다. 기록에는 원래 문장과 이름만 남기고 값은 남기지 않는다.
		p := m.params
		switch k.String() {
		case "esc":
			m.overlay, m.params = oNone, nil
			m.status = "취소함"
			return m, nil
		case "enter":
			p.vals = append(p.vals, m.input.Value())
			if len(p.vals) < len(p.names) {
				m.nextParam()
				return m, nil
			}
			m.overlay, m.params = oNone, nil
			args := make([]any, len(p.vals))
			run := p.run
			run.params = map[string]string{}
			for i, v := range p.vals {
				args[i] = v
				if remembered(p.names[i]) {
					run.params[p.names[i]] = v
				}
			}
			run.sql += "\n-- 파라미터: " + strings.Join(p.names, ", ")
			run.exec = execScript(m.clients, script{conn: m.conn(run.conn), stmts: []string{p.sql}, args: args, out: run.all,
				readOnlyTx: run.readOnly && m.prodLocked(run.conn)})
			return m, m.guard(run)
		}
		var cmd tea.Cmd
		m.input, cmd = m.input.Update(k)
		return m, cmd
	case oTx:
		// ─── 열린 트랜잭션 확인 창 ───
		// c·r 은 접속마다 COMMIT·ROLLBACK 을 보내고 끝나면 미뤄 둔 동작을 한다.
		p := m.txAsk
		if ks != "c" && ks != "r" {
			m.overlay, m.txAsk = oNone, nil
			m.status = "취소함"
			return m, nil
		}
		q := "COMMIT"
		if ks == "r" {
			q = "ROLLBACK"
		}
		m.overlay = oNone
		m.status = q + " 중…"
		conns := make([]db.Conn, len(p.conns))
		for i, c := range p.conns {
			conns[i] = m.conn(c)
		}
		// 종료·탭 닫기로 끊기기 전에 그 접속의 실행을 먼저 취소해 세션 잠금을 기다리지 않게 한다
		for _, t := range m.tabs {
			if t.running != nil && slices.Contains(p.conns, t.conn) {
				t.running()
				t.running, t.cancelled = nil, true
			}
		}
		prompt, clients, life := *p, m.clients, m.life
		return m, func() tea.Msg {
			ctx, cancel := context.WithTimeout(life, loadTimeout)
			defer cancel()
			var done []int
			for i, c := range conns {
				pg, err := clients.PG(ctx, c)
				var res *db.Result
				if err == nil {
					res, err = pg.Run(ctx, q)
				}
				if err == nil && q == "COMMIT" && res.Text != "COMMIT" {
					// 트랜잭션은 이미 되돌려져 끝났으므로 표시는 지우되 미뤄 둔 동작(종료·닫기)은 하지 않는다
					done = append(done, prompt.conns[i])
					err = errCommitRolledBack
				}
				if err != nil {
					return txDoneMsg{prompt: prompt, done: done, err: fmt.Errorf("%s %s: %w", q, c.Name, err)}
				}
				done = append(done, prompt.conns[i])
			}
			return txDoneMsg{prompt: prompt, done: done}
		}
	case oPrompt:
		switch ks {
		case "esc":
			m.overlay, m.promptDo = oNone, nil
			m.status = "취소함"
			return m, nil
		case "enter":
			do := m.promptDo
			m.overlay, m.promptDo = oNone, nil
			return m, do(m.input.Value())
		}
		var cmd tea.Cmd
		m.input, cmd = m.input.Update(k)
		return m, cmd
	case oImport:
		// ─── CSV 경로 입력 줄 ───
		// Enter 면 CSV 를 읽어 매핑 미리 보기와 함께 확인을 받는다.
		switch k.String() {
		case "esc":
			m.overlay = oNone
			return m, nil
		case "enter":
			m.overlay = oNone
		default:
			var cmd tea.Cmd
			m.input, cmd = m.input.Update(k)
			return m, cmd
		}
		n := m.importNode
		var cols []string
		if cat := m.catalogs[n.conn]; cat != nil {
			for _, s := range cat.Schemas {
				for _, tb := range s.Tables {
					if s.Name != n.schema || tb.Name != n.label {
						continue
					}
					for _, c := range tb.Columns {
						cols = append(cols, c.Name)
					}
				}
			}
		}
		if cols == nil {
			m.status = "열 목록을 아직 읽지 못함 (잠시 뒤 다시, 실패했으면 접속 줄에서 r)"
			return m, nil
		}
		path := m.input.Value()
		plan, err := readCSVPlan(path, cols)
		if err != nil {
			m.status = "CSV 읽기 실패: " + err.Error()
			return m, nil
		}
		ref := db.TableRef{Schema: n.schema, Table: n.label}
		spec := db.ImportSpec{Table: ref, Columns: plan.columns, Rows: plan.rows}
		t := m.cur()
		c, clients := m.conn(n.conn), m.clients
		return m, m.guard(pendingRun{tab: t.id, conn: n.conn, kind: rkImport, stmts: 1,
			confirm: fmt.Sprintf("%s 에 %d행 가져오기", ref.Quoted(), len(plan.rows)), preview: plan.preview,
			sql: fmt.Sprintf("-- CSV 가져오기 %s → %s (%d행)", filepath.Base(path), ref.Quoted(), len(plan.rows)),
			exec: func(ctx context.Context) (*db.Result, error) {
				pg, err := clients.PG(ctx, c)
				if err != nil {
					return nil, err
				}
				n, err := pg.ImportCSV(ctx, spec)
				if err != nil {
					return nil, err
				}
				return &db.Result{Text: fmt.Sprintf("%d행 가져옴", n)}, nil
			}})
	case oFinder:
		// ─── 찾기 창 ───
		var a finder.Action
		m.finder, a = m.finder.Update(k)
		if a.Close {
			m.overlay = oNone
		}
		if a.Chosen == nil {
			return m, nil
		}
		m.revealTab = 0
		return m, m.reveal(connByName(m.proj, a.Chosen.Conn), *a.Chosen)
	case oFilter:
		switch ks {
		case "esc":
			m.overlay = oNone
			return m, nil
		case "enter":
			m.overlay = oNone
			v := m.input.Value()
			n := m.current()
			if n != nil && m.isRedisKeyStage(n) {
				connNode := n
				for connNode.kind != nConn {
					connNode = connNode.parent
				}
				m.patterns[connNode.conn] = v
				connNode.loaded = false
				m.selectNode(connNode)
				return m, m.expand(connNode)
			}
			m.filter = v
			m.rebuild()
			return m, nil
		}
		var cmd tea.Cmd
		m.input, cmd = m.input.Update(k)
		return m, cmd
	case oPicker:
		list := m.pickerMatches()
		// 즐겨찾기 고르기의 d 는 거르기 글이 비었을 때만 지우기다(글자 d 로 거를 수 있게). Ctrl+D 는 늘 지우기다
		if m.pickDel != nil && (ks == "ctrl+d" || (ks == "d" && m.input.Value() == "")) {
			if m.pickerCursor >= len(list) {
				return m, nil
			}
			m.overlay = oNone
			return m, m.pickDel(list[m.pickerCursor])
		}
		switch ks {
		case "esc":
			m.overlay = oNone
			return m, nil
		case "down", "ctrl+n":
			m.pickerCursor = clamp(m.pickerCursor+1, len(list))
			return m, nil
		case "up", "ctrl+p":
			m.pickerCursor = clamp(m.pickerCursor-1, len(list))
			return m, nil
		case "enter":
			m.overlay = oNone
			if m.pickerCursor < 0 || m.pickerCursor >= len(list) {
				return m, nil
			}
			rel := list[m.pickerCursor]
			if m.pickDo != nil {
				return m, m.pickDo(rel)
			}
			cmd := m.openFile(rel)
			m.setFocus(fEditor)
			return m, cmd
		}
		var cmd tea.Cmd
		m.input, cmd = m.input.Update(k)
		m.pickerCursor = 0
		return m, cmd
	case oHistory:
		list := m.historyMatches()
		switch ks {
		case "esc":
			m.overlay = oNone
			return m, nil
		case "down", "ctrl+n":
			m.histCursor = clamp(m.histCursor+1, len(list))
			return m, nil
		case "up", "ctrl+p":
			m.histCursor = clamp(m.histCursor-1, len(list))
			return m, nil
		case "enter":
			m.overlay = oNone
			if m.histCursor >= len(list) {
				return m, nil
			}
			e := list[m.histCursor]
			if t.editor.Value() != "" {
				t = m.newTab()
			}
			t.editor.SetValue(e.SQL)
			cmd := m.edited(t)
			// 접속이 이미 있는 탭은 바꾸지 않는다. 다른 환경(prod 등)으로 몰래 옮기지 않기 위해서다
			for i, c := range m.proj.Conns {
				if t.conn < 0 && c.Name == e.Conn {
					cmd = tea.Batch(cmd, m.setConn(t, i))
				}
			}
			m.setFocus(fEditor)
			m.status = "기록을 편집기에 넣었음"
			return m, cmd
		}
		var cmd tea.Cmd
		m.input, cmd = m.input.Update(k)
		m.histCursor = 0
		return m, cmd
	}

	// 결과 영역이 입력 중이면(거르기·칸 편집·WHERE·값 편집) q 같은 글자 키도 전역 키보다 먼저 그쪽이 받는다
	if m.focus == fResult {
		switch {
		case t.redis != nil && t.redis.Editing():
			return m, m.redisEditKey(k)
		case t.grid.Typing():
			return m, m.gridKey(k)
		case t.tv != nil && t.tv.Typing():
			return m, m.tableKey(k)
		case t.planOn && planKeys[ks]:
			var a plantree.Action
			*t.plan, a = t.plan.Update(k)
			if a.Close {
				t.plan, t.planOn = nil, false
			}
			if ks == "t" {
				t.planOn = false
			}
			return m, nil
		}
	}

	// ─── 전역 ───
	switch ks {
	case "f1":
		m.overlay = oHelp
		return m, nil
	case "tab":
		// 자동완성 창·템플릿 펼치기·템플릿 자리 돌기는 편집기가 Tab 을 쓴다
		if m.focus != fEditor || !t.editor.WantsTab() {
			m.setFocus((m.focus + 1) % 3)
			return m, nil
		}
	case "shift+tab":
		m.setFocus((m.focus + 2) % 3)
		return m, nil
	case "ctrl+t":
		m.newTab()
		m.setFocus(fEditor)
		return m, nil
	case "ctrl+w":
		// 고정한 탭은 실수로 닫지 않게 묻는다(R57)
		if t.pinned {
			m.confirm(fmt.Sprintf("고정한 탭 %s 닫기", t.displayName()), func() tea.Cmd { return m.closeAsk(t) })
			return m, nil
		}
		return m, m.closeAsk(t)
	case "alt+r":
		// ─── 탭 이름 (R57) ───
		m.prompt("탭 이름 (비우면 원래 이름) ", t.name, func(v string) tea.Cmd {
			t.name = strings.TrimSpace(v)
			m.status = "탭 이름: " + t.displayName()
			return nil
		})
		return m, nil
	case "alt+p":
		// ─── 탭 고정 (R57) ───
		// 고정한 탭은 앞에 모은다. 탭 순서가 곧 Alt+N·마우스 순서라 m.tabs 를 다시 늘어놓는다
		t.pinned = !t.pinned
		slices.SortStableFunc(m.tabs, pinnedFirst)
		m.tab = slices.Index(m.tabs, t)
		m.status = "탭 고정 풂: " + t.displayName()
		if t.pinned {
			m.status = "탭 고정: " + t.displayName()
		}
		return m, nil
	case "ctrl+u":
		return m, m.userLookup()
	case "ctrl+l":
		return m, m.toggleLock()
	case "alt+c":
		return m, m.compareConns()
	case "alt+1", "alt+2", "alt+3", "alt+4", "alt+5", "alt+6", "alt+7", "alt+8", "alt+9":
		m.switchTab(int(ks[len(ks)-1] - '1'))
		return m, nil
	case "ctrl+pgup":
		m.switchTab((m.tab + len(m.tabs) - 1) % len(m.tabs))
		return m, nil
	case "ctrl+pgdown":
		m.switchTab((m.tab + 1) % len(m.tabs))
		return m, nil
	case "ctrl+left", "ctrl+right":
		// 편집기에서는 단어 이동이라 영역 크기는 트리·결과 포커스에서만 바꾼다
		// 그려진 크기에서 움직여, 창이 작아 줄여 그린 크기에서도 한 번에 반응한다. 그려진 크기가 그대로면(범위 끝)
		// 저장한 크기를 건드리지 않는다. 키를 누르고 있어도 마지막 키 뒤 한 번만 저장한다(R46.2)
		if m.focus != fEditor {
			step := treeWidthStep
			if ks == "ctrl+left" {
				step = -step
			}
			l := m.layout()
			if w := m.clampTree(l.treeW + step); w != l.treeW {
				m.treeW = w
				m.resize()
				return m, m.queuePaneSave()
			}
			return m, nil
		}
	case "ctrl+up", "ctrl+down":
		if m.focus != fEditor {
			step := editorStep
			if ks == "ctrl+up" {
				step = -step
			}
			l := m.layout()
			if h := m.clampEditor(l.edH + step); h != l.edH {
				m.edH = h
				m.resize()
				return m, m.queuePaneSave()
			}
			return m, nil
		}
	case "ctrl+r", "f5":
		cmd := tea.Batch(m.run(m.target(t)), m.ensureCatalog(t.conn))
		// 파일 없는 탭의 첫 실행은 글을 queries/ 에 저장하고 탭을 그 파일에 잇는다(R45.1). 실행 안내 앞에 경로를 보인다
		if autoSaveOn() && m.savableNew(t) {
			rel, err := m.saveNew(t)
			note := "저장함: " + rel
			if err != nil {
				note = "새 쿼리 저장 실패: " + err.Error()
			}
			m.status = strings.TrimSuffix(note+" · "+m.status, " · ")
		}
		m.touch(t)
		return m, cmd
	case "ctrl+e", "alt+e":
		// ANALYZE 는 문장을 실제로 실행하므로 그 문장이 읽기 전용이 아니면 모든 환경에서 묻는다(R9)
		if t.conn < 0 || m.conn(t.conn).Kind != db.KindPG {
			m.status = "EXPLAIN 은 PostgreSQL 접속에서만 됨"
			return m, nil
		}
		stmts := db.SplitStatements(m.target(t))
		if len(stmts) != 1 {
			m.status = "EXPLAIN 할 문장 하나에 커서를 두거나 고르세요"
			return m, nil
		}
		analyze := ks == "alt+e"
		opt := db.ExplainOpt{Analyze: analyze, Buffers: analyze, Format: "JSON"}
		// 기록에는 확장 전 글로 감싼 문장(q)을 남기고 보내는 것만 enc() 를 바꾼다(R52)
		orig := stmts[0]
		sent, note, err := m.expandEnc(t.conn, orig)
		if err != nil {
			m.status = err.Error()
			return m, nil
		}
		q := db.ExplainSQL(orig, opt)
		// ANALYZE 없이 감싼 EXPLAIN 은 속 문장을 실행하지 않아 늘 읽기다. 감싼 글로 판정하면 속 문장의 analyze 글자에 걸린다.
		// ANALYZE 로 감쌌거나 직접 쓴 EXPLAIN(감싸지 않음)이면 원래 문장으로 가른다
		readOnly := true
		if analyze || db.FirstSQLWord(sent) == "explain" {
			readOnly = db.IsReadOnlySQL(sent)
		}
		// ANALYZE 는 속 문장을 실제로 실행하므로 위험 문장 판정(R25)도 속 문장으로 한다
		danger := ""
		if analyze {
			danger = db.DangerOf(sent)
		}
		// 읽기로 판정한 ANALYZE 도 속 문장을 실행한다. 잠긴 prod 면 READ ONLY 트랜잭션으로 돌려 CTE 안 쓰기를 서버가 막게 한다(R54)
		return m, m.guard(pendingRun{tab: t.id, conn: t.conn, readOnly: readOnly, analyze: !readOnly, danger: danger, sql: q, stmts: 1, note: note,
			kind: rkPlan, tx: true, exec: execScript(m.clients, script{conn: m.conn(t.conn), stmts: []string{db.ExplainSQL(sent, opt)},
				readOnlyTx: readOnly && m.prodLocked(t.conn)})})
	case "f8":
		// ─── F8 정렬 ───
		// 선택이 있으면 선택만, 없으면 버퍼 전체를 정렬한다.
		// 한계: 편집기에 범위 바꾸기 API 가 없어 SetValue 로 넣으므로 되돌리기 기록과 커서 위치가 처음으로 돌아간다.
		if t.conn >= 0 && m.conn(t.conn).Kind == db.KindRedis {
			m.status = "Redis 명령은 정렬하지 않음"
			return m, nil
		}
		text := t.editor.Value()
		out := sqlfmt.Format(text)
		if s, e, ok := t.editor.Selection(); ok {
			out = text[:s] + sqlfmt.Format(text[s:e]) + text[e:]
		}
		if out == text {
			m.status = "이미 정렬됨"
			return m, nil
		}
		t.editor.SetValue(out)
		m.status = m.label("SQL 정렬함 (F8)")
		return m, m.edited(t)
	case "f9", "f10", "f11":
		// ─── F9·F10·F11 트랜잭션 ───
		// F9 는 Auto/Manual 전환, F10 은 COMMIT, F11 은 ROLLBACK 이다. prod COMMIT 은 쓰기 확인을 받는다.
		if t.conn < 0 || m.conn(t.conn).Kind != db.KindPG {
			m.status = "트랜잭션 모드는 PostgreSQL 접속에서만 됨"
			return m, nil
		}
		if ks == "f9" {
			m.manual[t.conn] = !m.manual[t.conn]
			m.status = "Auto: 문장마다 바로 반영"
			if m.manual[t.conn] {
				m.status = m.label("Manual: 실행 전에 BEGIN, F10 COMMIT · F11 ROLLBACK")
			}
			return m, nil
		}
		if !m.inTx[t.conn] {
			m.status = "열린 트랜잭션 없음"
			return m, nil
		}
		q := "COMMIT"
		if ks == "f11" {
			q = "ROLLBACK"
		}
		// ROLLBACK 은 바꾼 것을 버릴 뿐이라 prod 에서도 묻지 않는다
		return m, m.guard(pendingRun{tab: t.id, conn: t.conn, kind: rkTx, readOnly: ks == "f11", sql: q, stmts: 1,
			exec: execScript(m.clients, script{conn: m.conn(t.conn), stmts: []string{q}})})
	case "esc":
		// 내보내기 취소는 esc 가 다른 일(자동완성 닫기·템플릿 자리·편집기 선택 풀기·차트·값 보기·표 선택 끝내기·값 편집기 닫기·거르기 해제)을 하지 않을 때만 가로챈다
		_, _, selected := t.editor.Selection()
		other := (m.focus == fEditor && (t.editor.WantsTab() || selected)) ||
			(m.focus == fResult && (t.grid.Viewing() || t.grid.Typing() || t.redis != nil)) ||
			(m.focus == fTree && m.filter != "")
		// 키 일괄 삭제(R42)도 같은 규칙으로 멈춘다. 둘 다 돌고 있으면 한 번에 멈춘다
		if (m.export != nil || m.unlink != nil) && !other {
			m.status = ""
			if m.export != nil {
				m.export.canceled = true
				m.export.cancel()
				m.status = "전체 내보내기 취소 요청함"
			}
			if m.unlink != nil {
				m.unlink.canceled = true
				m.unlink.cancel()
				m.status = strings.TrimPrefix(m.status+" · 키 지우기 멈춤 요청함", " · ")
			}
			return m, nil
		}
	case "ctrl+p":
		// ─── 명령 팔레트 (R40) ───
		// F1 도움말 항목을 이름·키로 거르고, 고르면 그 항목의 포커스로 옮겨 키를 누른 것처럼 다시 넣는다.
		// 지금 쓸 수 없는 동작도 그대로 눌러 그 동작의 안내가 상태 줄에 나오게 한다. 자동완성 창에서는 위로 이동이다
		// 동작 앞에 PostgreSQL 테이블·뷰를 둔다(R40.2). 최근 연 것이 최신순으로 먼저고 빈 팔레트에는 그 앞 10개만 보인다.
		// 거르면 Ctrl+N 과 같은 카탈로그의 나머지 테이블이 환경(local·stg·prod) 순으로 최근 것 뒤에 붙는다
		if m.focus == fEditor && t.editor.PopupOpen() {
			break
		}
		var labels []string
		tables := map[string]paletteTable{}
		// 같은 테이블은 접속 인덱스로 하나로 합친다. 이름이 같은 접속(DataGrip 은 막지 않는다)의 줄은 글이 겹치면 #자리를 붙여 가른다
		type tableKey struct {
			conn          int
			schema, table string
		}
		seen := map[tableKey]bool{}
		for _, r := range m.sess.RecentTables {
			ci, ok := m.proj.ConnIndex(r.Conn)
			if !ok || seen[tableKey{ci, r.Schema, r.Table}] { // 지금 .idea 에 없는 접속의 기록은 빼고 보인다
				continue
			}
			seen[tableKey{ci, r.Schema, r.Table}] = true
			label := m.tableLabel(ci, r.Schema, r.Table)
			if _, dup := tables[label]; dup {
				label += fmt.Sprintf(" #%d", ci+1)
			}
			it := finder.Item{Conn: m.conn(ci).Name, Kind: finder.KindTable, Schema: r.Schema, Name: r.Table}
			tables[label] = paletteTable{it: it, conn: ci, quiet: len(tables) >= paletteRecent}
			labels = append(labels, label)
		}
		for _, env := range paletteEnvs {
			for i, c := range m.proj.Conns {
				cat := m.catalogs[i]
				if cat == nil || c.Env() != env {
					continue
				}
				for _, s := range cat.Schemas {
					for _, tb := range s.Tables {
						if seen[tableKey{i, s.Name, tb.Name}] {
							continue
						}
						seen[tableKey{i, s.Name, tb.Name}] = true
						label := m.tableLabel(i, s.Name, tb.Name)
						if _, dup := tables[label]; dup {
							label += fmt.Sprintf(" #%d", i+1)
						}
						tables[label] = paletteTable{it: finder.Item{Conn: c.Name, Kind: finder.KindTable, Schema: s.Name, Name: tb.Name}, conn: i, quiet: true}
						labels = append(labels, label)
					}
				}
			}
		}
		acts := map[string]paletteAct{}
		for _, sec := range helpSections {
			for _, kv := range sec.keys {
				if kv[2] == "" {
					continue
				}
				// 고르면 보이는 글자(⌘)가 아니라 원래 키 이름(kv[2])을 누른다(R47.1)
				label := fmt.Sprintf("%s — %s · %s", m.label(kv[1]), m.label(kv[0]), sec.title)
				labels = append(labels, label)
				acts[label] = paletteAct{at: sec.at, k: keyMsgOf(kv[2])}
			}
		}
		m.openPicker("명령 ", labels, nil, func(label string) tea.Cmd {
			// 테이블은 Ctrl+N 처럼 트리에서 골라 연다. 탭 규칙(R3.1)·접속 바꾸기·트랜잭션 확인은 트리 Enter 와 같다
			if tb, ok := tables[label]; ok {
				return m.reveal(tb.conn, tb.it)
			}
			a := acts[label]
			if a.at != fKeep {
				m.setFocus(a.at)
			}
			_, cmd := m.Update(a.k)
			return cmd
		}, nil)
		m.pickerTables = tables
		return m, nil
	case "ctrl+b", "f12":
		// ─── 정의로 이동 (R26) ───
		// 커서 아래 이름을 활성 PostgreSQL 접속에서 찾는다. 결과는 defMsg 에서 연다.
		if m.focus != fEditor {
			break
		}
		if t.conn < 0 || m.conn(t.conn).Kind != db.KindPG {
			m.status = "정의로 이동은 PostgreSQL 접속에서만 됨"
			return m, nil
		}
		name := t.editor.IdentAt()
		if name == "" {
			m.status = "커서 아래 이름이 없음"
			return m, nil
		}
		id, conn := t.id, t.conn
		return m, withPG(m.life, m.clients, m.conn(conn), func(ctx context.Context, pg *db.PGConn, err error) tea.Msg {
			if err != nil {
				return defMsg{tab: id, conn: conn, name: name, err: err}
			}
			obj, err := pg.Resolve(ctx, name)
			return defMsg{tab: id, conn: conn, name: name, obj: obj, err: err}
		})
	case "f4":
		// ─── 즐겨찾기 저장 (R29.1) ───
		// 선택(없으면 편집기 전체)을 이름을 받아 저장한다. 같은 이름이면 덮어쓸지 묻는다.
		text := t.editor.Value()
		if s, e, ok := t.editor.Selection(); ok {
			text = text[s:e]
		}
		if strings.TrimSpace(text) == "" {
			m.status = "즐겨찾기에 저장할 글이 없음"
			return m, nil
		}
		uuid := ""
		if t.conn >= 0 {
			uuid = m.conn(t.conn).UUID
		}
		m.prompt("즐겨찾기 이름 ", "", func(v string) tea.Cmd {
			name, err := favorites.NormalizeName(v)
			if err != nil {
				m.status = "즐겨찾기 이름: " + err.Error()
				return nil
			}
			store, err := favorites.Load(m.proj.Dir)
			if err != nil {
				m.status = "즐겨찾기 읽기 실패: " + err.Error()
				return nil
			}
			save := func() tea.Cmd {
				if err := store.Put(favorites.Fav{Name: name, Conn: uuid, SQL: text}); err != nil {
					m.status = "즐겨찾기 저장 실패: " + err.Error()
					return nil
				}
				m.status = "즐겨찾기 저장함: " + name
				return nil
			}
			if _, ok := store.Get(name); ok {
				m.confirm(fmt.Sprintf("즐겨찾기 %q 덮어쓸까요", name), save)
				return nil
			}
			return save()
		})
		return m, nil
	case "f3":
		// ─── 즐겨찾기 열기 (R29.2) ───
		// Enter 는 새 탭에 글을 넣고 저장한 접속(없으면 지금 접속)을 활성으로 바꾼다. d 는 묻고 지운다.
		store, err := favorites.Load(m.proj.Dir)
		if err != nil {
			m.status = "즐겨찾기 읽기 실패: " + err.Error()
			return m, nil
		}
		favs := store.List()
		if len(favs) == 0 {
			m.status = m.label("즐겨찾기가 없음 (F4 로 저장)")
			return m, nil
		}
		names, conns := make([]string, len(favs)), map[string]string{}
		for i, f := range favs {
			names[i], conns[f.Name] = f.Name, f.Conn
		}
		m.openPicker("즐겨찾기 ", names, conns, func(name string) tea.Cmd {
			f, ok := store.Get(name)
			if !ok {
				return nil
			}
			nt := m.newTab()
			nt.title = f.Name
			nt.editor.SetValue(f.SQL)
			var cmd tea.Cmd
			if i, ok := m.proj.ConnIndex(f.Conn); ok {
				cmd = m.setConn(nt, i)
			}
			m.setFocus(fEditor)
			m.status = "즐겨찾기를 새 탭에 열었음: " + f.Name
			return cmd
		}, func(name string) tea.Cmd {
			m.confirm(fmt.Sprintf("즐겨찾기 %q 지울까요", name), func() tea.Cmd {
				if err := store.Delete(name); err != nil {
					m.status = "즐겨찾기 지우기 실패: " + err.Error()
					return nil
				}
				m.status = "즐겨찾기 지움: " + name
				return nil
			})
			return nil
		})
		return m, nil
	case "f6":
		// ─── 스키마 고르기 (R27.2) ───
		if t.conn < 0 || m.conn(t.conn).Kind != db.KindPG {
			m.status = "스키마 고르기는 PostgreSQL 접속에서만 됨"
			return m, nil
		}
		conn, c, clients, life := t.conn, m.conn(t.conn), m.clients, m.life
		do := func(schema string) tea.Cmd {
			m.status = "search_path 바꾸는 중…"
			return withPG(life, clients, c, func(ctx context.Context, pg *db.PGConn, err error) tea.Msg {
				if err == nil {
					err = pg.SetSearchPath(ctx, schema)
				}
				return searchPathMsg{conn: conn, schema: schema, err: err}
			})
		}
		return m, withPG(life, clients, c, func(ctx context.Context, pg *db.PGConn, err error) tea.Msg {
			if err != nil {
				return pickListMsg{conn: conn, err: err, do: do}
			}
			ss, err := pg.FindSchemas(ctx)
			return pickListMsg{conn: conn, prompt: "스키마 ", items: ss, err: err, do: do}
		})
	case "f7":
		// ─── 전체 내보내기 (R33.1) ───
		// 마지막 표 실행의 문장(테이블 보기면 WHERE·정렬 그대로)을 행 제한 없이 COPY 로 파일에 쓴다.
		if m.export != nil {
			m.status = "전체 내보내기 중 (esc 취소)"
			return m, nil
		}
		if t.view >= 0 {
			m.status = m.label("고정·문장별 결과는 전체 내보내지 않음 ({ } 로 마지막 결과를 고른 뒤 F7)")
			return m, nil
		}
		sql, conn := t.exportSQL, t.exportConn
		if t.tv != nil {
			if t.viewDB != m.conn(t.viewConn).DB {
				m.status = "테이블 보기를 연 뒤 데이터베이스를 바꿔 전체 내보내지 않음 (다시 여세요)"
				return m, nil
			}
			// 테이블 보기 SQL 끝의 페이지 자르기만 떼어 WHERE·정렬은 그대로 둔다
			off, size := t.tv.Page()
			sql, conn = strings.TrimSuffix(t.tv.SQL(), fmt.Sprintf(" LIMIT %d OFFSET %d", size, off)), t.viewConn
		}
		stmts := db.SplitStatements(sql)
		switch {
		case len(stmts) == 0 || conn < 0 || m.conn(conn).Kind != db.KindPG:
			m.status = m.label("전체 내보낼 표 결과가 없음 (PostgreSQL 에서 실행한 뒤 F7)")
			return m, nil
		case len(stmts) > 1:
			m.status = "문장 하나만 전체 내보내기 (커서 문장으로 다시 실행)"
			return m, nil
		case !db.IsReadOnlySQL(stmts[0]):
			m.status = "읽기 문장만 전체 내보내기"
			return m, nil
		}
		if _, names := params.Bind(stmts[0]); len(names) > 0 {
			m.status = "파라미터(:name·$n)가 있는 문장은 전체 내보내지 않음"
			return m, nil
		}
		// 표를 채운 문장은 기록에 남긴 확장 전 글이라 보내기 전에 enc() 를 다시 바꾼다(R52)
		q, note, err := m.expandEnc(conn, stmts[0])
		if err != nil {
			m.status = err.Error()
			return m, nil
		}
		ctx, cancel := context.WithCancel(m.life)
		job := &exportJob{cancel: cancel, start: time.Now(), conn: conn}
		m.export = job
		name, c, clients := exportName(t.title, "csv"), m.conn(conn), m.clients
		m.status = strings.TrimPrefix(note+" · 전체 내보내는 중… (esc 취소)", " · ")
		return m, tea.Batch(tea.Tick(exportTick, func(time.Time) tea.Msg { return exportTickMsg{job: job} }), func() tea.Msg {
			defer cancel()
			f, err := exportFile(name)
			if err != nil {
				return exportDoneMsg{job: job, err: err}
			}
			w := bufio.NewWriter(f)
			var rows int64
			pg, err := clients.PG(ctx, c)
			if err == nil {
				rows, err = pg.CopyOut(ctx, q, w, job.bytes.Store)
			}
			if err = errors.Join(err, w.Flush(), f.Close()); err != nil {
				_ = os.Remove(f.Name()) // 끊긴 파일을 완성본으로 잘못 쓰지 않게 지운다. 못 지워도 알릴 오류는 위의 것이다
				return exportDoneMsg{job: job, err: err}
			}
			return exportDoneMsg{job: job, rows: rows, path: f.Name()}
		})
	case "ctrl+n":
		// ─── Ctrl+N 찾기 ───
		// 목록을 읽은 PostgreSQL 접속의 테이블·뷰·열·함수(R56)와 트리에 읽혀 있는 Redis 키를 후보로 둔다.
		// 열·함수는 테이블·뷰 뒤에 둬 같은 점수면 테이블이 먼저 나온다. 자동완성 창에서는 Ctrl+P 처럼 후보 이동이다
		if m.focus == fEditor && t.editor.PopupOpen() {
			break
		}
		var items, more []finder.Item
		for i, c := range m.proj.Conns {
			if cat := m.catalogs[i]; cat != nil {
				for _, s := range cat.Schemas {
					for _, t := range s.Tables {
						kind := finder.KindTable
						if t.View {
							kind = finder.KindView
						}
						items = append(items, finder.Item{Conn: c.Name, Kind: kind, Schema: s.Name, Name: t.Name})
						for _, col := range t.Columns {
							more = append(more, finder.Item{Conn: c.Name, Kind: finder.KindColumn, Schema: s.Name, Name: t.Name + "." + col.Name})
						}
					}
				}
				for _, f := range cat.Functions {
					more = append(more, finder.Item{Conn: c.Name, Kind: finder.KindFunction, Name: f})
				}
			}
			if cn := m.connNode(i); c.Kind == db.KindRedis && cn != nil {
				for _, k := range redisKeys(cn) {
					items = append(items, finder.Item{Conn: c.Name, Kind: finder.KindRedisKey, Name: k})
				}
			}
		}
		items = append(items, more...)
		if len(items) == 0 {
			m.status = "찾을 객체가 없음 (PostgreSQL 접속을 한 번 고르면 목록을 읽음)"
			return m, nil
		}
		m.finder = finder.New(items)
		m.finder.SetSize(min(m.w-4, 100), min(m.h-4, 30))
		m.overlay = oFinder
		return m, nil
	case "ctrl+h":
		if m.hist == nil {
			m.status = "실행 기록을 열지 못해 쓸 수 없음"
			return m, nil
		}
		m.histCursor = 0
		m.input.SetValue("")
		m.input.Prompt = "기록 "
		m.input.Focus()
		m.overlay = oHistory
		return m, nil
	case "ctrl+o":
		qs, err := listQueries(m.proj.Dir)
		if err != nil {
			m.status = "queries 목록 실패: " + err.Error()
			return m, nil
		}
		// 트리(R43)와 같은 최근 수정 순이다
		files := make([]string, len(qs))
		for i, q := range qs {
			files[i] = q.rel
		}
		m.openPicker("파일 ", files, m.proj.Mapping, nil, nil)
		return m, nil
	case "ctrl+s":
		// 보류 중인 표 편집이 있고 결과 영역에 있으면 제출이다. 아니면 파일 저장
		if m.focus == fResult && t.grid.Pending() > 0 {
			return m, m.gridKey(k)
		}
		if t.file == "" {
			m.status = m.label("연 파일이 없어 저장하지 않음 (Ctrl+O 로 열기)")
			return m, nil
		}
		// 밖에서 바뀌어 자동 저장을 멈춘 파일도 덮어쓰고, 지워진 파일은 다시 만든다(R45.2)
		err := m.writeTabFile(t)
		note := "저장함: "
		if errors.Is(err, fs.ErrNotExist) {
			err, note = m.writeNew(t, t.file, ""), "지워진 파일을 다시 만듦: "
		}
		m.refreshStale()
		switch {
		case errors.Is(err, errOutside):
			t.autoStop = true
			m.status = autoSaveOutside + " (끝낼 때 충돌 사본으로 남김)"
		case err != nil:
			m.status = "저장 실패: " + err.Error()
		default:
			m.status = note + t.file
		}
		return m, nil
	case "q":
		if m.focus != fEditor {
			return m, m.quitAsk()
		}
	}

	// ─── 포커스 영역 ───
	switch m.focus {
	case fEditor:
		var cmd tea.Cmd
		before := t.editor.Value()
		t.editor, cmd = t.editor.Update(k)
		// 사용자가 친 글이다(R45.1). 파일에 이은 탭은 마지막 고침 1초 뒤 자동 저장한다(R45.2)
		if t.editor.Value() != before {
			t.userText = true
			cmd = tea.Batch(cmd, m.edited(t))
		}
		return m, cmd
	case fResult:
		// ─── 결과 영역 ───
		// 값 편집기·테이블 보기·결과 고정 키를 먼저 보고 나머지는 표로 넘긴다.
		t := m.cur()
		ks := k.String()
		if t.redis != nil {
			if ks == "esc" {
				t.redis = nil
				return m, nil
			}
			return m, m.redisEditKey(k)
		}
		// 시트에서 복사한 칸(⌘V·bracketed paste)은 표에 붙인다(R48)
		if k.Paste {
			if p, err := t.grid.Paste(string(k.Runes)); err != nil {
				m.status = err.Error()
			} else {
				m.status = p.Status()
			}
			return m, nil
		}
		if ks == "ctrl+z" {
			return m, m.undoLast(t)
		}
		if t.plan != nil && !t.planOn && ks == "t" && !t.grid.Viewing() {
			t.planOn = true
			return m, nil
		}
		switch ks {
		case "p":
			// 고정한 결과는 다음 실행에도 남는다
			pinned := 0
			for _, s := range t.subs {
				if s.pinned {
					pinned++
				}
			}
			switch {
			case t.view >= 0 && t.subs[t.view].pinned:
				m.status = "이미 고정한 결과"
			case pinned >= maxPins:
				m.status = fmt.Sprintf("고정은 탭마다 %d개까지 (P 로 풀기)", maxPins)
			case t.view >= 0:
				t.subs[t.view].pinned = true
				m.status = "고정함: " + t.subs[t.view].title
			case t.res == nil:
				m.status = "고정할 결과가 없음"
			default:
				t.subs = append(t.subs, subResult{title: fmt.Sprintf("고정 %d", pinned+1), res: t.res, pinned: true})
				m.status = fmt.Sprintf("고정함 ({ } 로 고르기, %d/%d)", pinned+1, maxPins)
			}
			return m, nil
		case "P":
			if t.view < 0 || !t.subs[t.view].pinned {
				m.status = "고정한 결과를 고른 뒤 P 로 풉니다"
				return m, nil
			}
			if n := t.grid.Pending(); n > 0 {
				m.status = fmt.Sprintf(m.label("변경 %d건을 먼저 제출(Ctrl+S)하거나 되돌리세요(u)"), n)
				return m, nil
			}
			t.subs = append(t.subs[:t.view], t.subs[t.view+1:]...)
			t.view = -1
			t.grid.SetResult(t.res)
			m.status = "고정 풂"
			return m, nil
		case "{", "}":
			// 보류 중인 편집은 다른 결과로 바꾸면 사라지므로 막는다
			if len(t.subs) == 0 {
				m.status = "고정·문장별 결과가 없음 (p 로 고정)"
				return m, nil
			}
			if n := t.grid.Pending(); n > 0 {
				m.status = fmt.Sprintf(m.label("변경 %d건을 먼저 제출(Ctrl+S)하거나 되돌리세요(u)"), n)
				return m, nil
			}
			n := len(t.subs) + 1
			d := 1
			if ks == "{" {
				d = -1
			}
			t.view = (t.view+1+d+n)%n - 1
			t.grid.SetResult(t.shown())
			return m, nil
		case "K", "X":
			// ─── 백엔드 신호 ───
			// pid 열이 있는 결과(Activity·Locks 등)에서 고른 행의 백엔드에 K 취소·X 종료를 보낸다. 늘 묻는다.
			if m.staleView(t) {
				return m, nil
			}
			res := t.shown()
			col := -1
			if res != nil {
				col = slices.Index(res.Cols, "pid")
			}
			row := t.grid.CurrentRow()
			if col < 0 || row == nil || t.conn < 0 || m.conn(t.conn).Kind != db.KindPG {
				m.status = "K(취소)·X(종료)는 pid 열이 있는 결과(A 활동 등)에서 누르세요"
				return m, nil
			}
			pid, err := strconv.Atoi(row[col])
			if err != nil {
				m.status = "pid 를 읽지 못함: " + row[col]
				return m, nil
			}
			verb, terminate := "쿼리 취소", ks == "X"
			if terminate {
				verb = "연결 종료"
			}
			c, clients := m.conn(t.conn), m.clients
			return m, m.guard(pendingRun{tab: t.id, conn: t.conn, kind: rkSignal, confirm: fmt.Sprintf("pid %d %s", pid, verb),
				exec: func(ctx context.Context) (*db.Result, error) {
					pg, err := clients.PG(ctx, c)
					if err != nil {
						return nil, err
					}
					if terminate {
						err = pg.TerminateBackend(ctx, pid)
					} else {
						err = pg.CancelBackend(ctx, pid)
					}
					if err != nil {
						return nil, err
					}
					return &db.Result{Text: fmt.Sprintf("pid %d %s", pid, verb)}, nil
				}})
		}
		if t.tv != nil {
			return m, m.tableKey(k)
		}
		return m, m.gridKey(k)
	}

	// ─── 트리 ───
	n := m.current()
	switch ks {
	case "j", "down":
		m.cursor = clamp(m.cursor+1, len(m.rows))
	case "k", "up":
		m.cursor = clamp(m.cursor-1, len(m.rows))
	case "g", "home":
		m.cursor = 0
	case "G", "end":
		m.cursor = clamp(len(m.rows)-1, len(m.rows))
	case "?":
		m.overlay = oHelp
	case "/":
		m.overlay = oFilter
		m.input.Prompt = "/ "
		m.input.SetValue(m.filter)
		if n != nil && m.isRedisKeyStage(n) {
			m.input.Prompt = "SCAN MATCH "
			m.input.SetValue(m.patterns[n.conn])
		}
		m.input.CursorEnd()
		m.input.Focus()
	case "esc":
		if m.filter != "" {
			m.filter = ""
			m.rebuild()
		}
	case "h", "left":
		if n == nil {
			break
		}
		if n.expanded && n.expandable() {
			n.expanded = false
			m.rebuild()
		} else if n.parent != nil {
			m.selectNode(n.parent)
		}
	case "r":
		if n == nil {
			break
		}
		// 쿼리 파일 목록은 디스크에서 다시 읽기만 하면 된다(R43). 접속 줄의 r 도 다시 붙일 목록을 새로 읽는다
		m.refreshQueries()
		if n.kind == nQueries || n.kind == nFile {
			return m, nil
		}
		target := n
		if !n.expandable() && n.parent != nil {
			target = n.parent
		}
		// Redis 폴더는 키 목록을 나눠 보인 것이라 접속 단위로 다시 SCAN 한다
		for m.isRedisKeyStage(target) && target.kind != nConn {
			target = target.parent
		}
		target.loaded = false
		cmd := m.expand(target)
		if target.kind == nConn {
			delete(m.catalogs, target.conn)
			cmd = tea.Batch(cmd, m.ensureCatalog(target.conn))
		}
		return m, cmd
	case "d", "D":
		if n == nil || n.kind != nTable {
			m.status = "d(구조)·D(DDL)는 테이블에서 누르세요"
			break
		}
		if ks == "d" {
			return m, m.describe(t, n)
		}
		ref, conn := db.TableRef{Schema: n.schema, Table: n.label}, n.conn
		return m, withPG(m.life, m.clients, m.conn(conn), func(ctx context.Context, pg *db.PGConn, err error) tea.Msg {
			if err != nil {
				return ddlMsg{conn: conn, err: err}
			}
			ddl, err := pg.TableDDL(ctx, ref)
			return ddlMsg{conn: conn, table: ref.Table, ddl: ddl, err: err}
		})
	case "A", "L", "S":
		// ─── 운영 조회 ───
		// PostgreSQL 접속 줄은 A(Activity)·L(Locks)·S(TableSizes), Redis 접속 줄은 A(INFO)·L(SLOWLOG)·S(큰 키)다(R22.1·R34.1).
		// 편집기 세션이 아니라 풀에서 읽는 읽기라 prod 에서도 묻지 않는다.
		if n == nil || n.kind != nConn {
			m.status = "A·L·S(운영 조회)는 접속 줄에서 누르세요"
			return m, nil
		}
		t := m.cur()
		if n.conn != t.conn && t.bound() {
			t = m.newTab()
		}
		c, clients := m.conn(n.conn), m.clients
		return m, m.guard(pendingRun{tab: t.id, conn: n.conn, readOnly: true, retarget: true, exec: func(ctx context.Context) (*db.Result, error) {
			if c.Kind == db.KindRedis {
				rc, err := clients.Redis(ctx, c)
				if err != nil {
					return nil, err
				}
				switch ks {
				case "A":
					return rc.InfoSummary(ctx)
				case "L":
					return rc.SlowLog(ctx, redisSlowLogN)
				}
				res, err := rc.BigKeys(ctx, redisBigKeySample, redisBigKeyTop)
				if errors.Is(err, db.ErrNoMemoryUsage) {
					return nil, errors.New("이 서버는 MEMORY USAGE 를 쓸 수 없어 큰 키를 셀 수 없음")
				}
				return res, err
			}
			pg, err := clients.PG(ctx, c)
			if err != nil {
				return nil, err
			}
			switch ks {
			case "A":
				return pg.Activity(ctx)
			case "L":
				return pg.Locks(ctx)
			}
			return pg.TableSizes(ctx)
		}})
	case "B":
		// ─── 데이터베이스 고르기 (R27.1) ───
		// 같은 서버의 데이터베이스 목록을 보이고, 고르면 이 접속을 그 DB 로 다시 연다(.idea 는 그대로).
		if n == nil || n.kind != nConn || m.conn(n.conn).Kind != db.KindPG {
			m.status = "B(데이터베이스)는 PostgreSQL 접속 줄에서 누르세요"
			return m, nil
		}
		conn := n.conn
		do := func(name string) tea.Cmd {
			if name == m.conn(conn).DB {
				m.status = "이미 그 데이터베이스: " + name
				return nil
			}
			if why := m.dbSwitchBlock(conn); why != "" {
				m.status = why
				return nil
			}
			// 세션을 닫으면 열린 트랜잭션이 몰래 되돌려지므로 다른 탭이 같은 접속을 써도 먼저 묻는다(R17.3)
			if m.inTx[conn] {
				return m.askTx(txPrompt{tab: m.cur().id, conns: []int{conn}, after: afterDB, db: name})
			}
			return m.switchDB(conn, name)
		}
		return m, withPG(m.life, m.clients, m.conn(conn), func(ctx context.Context, pg *db.PGConn, err error) tea.Msg {
			if err != nil {
				return pickListMsg{conn: conn, err: err, do: do}
			}
			names, err := pg.Databases(ctx)
			return pickListMsg{conn: conn, prompt: "데이터베이스 ", items: names, err: err, do: do}
		})
	case "F":
		// ─── 데이터 검색 (R33.2) ───
		// 찾을 글을 받아 스키마(접속 줄이면 search_path 첫 스키마)의 글자 열을 찾는다. prod 는 테이블 수를 세어 먼저 묻는다.
		if n == nil || (n.kind != nConn && n.kind != nSchema) || m.conn(n.conn).Kind != db.KindPG {
			m.status = "F(데이터 검색)는 PostgreSQL 접속·스키마 줄에서 누르세요"
			return m, nil
		}
		schema := ""
		if n.kind == nSchema {
			schema = n.label
		}
		conn, c, clients, life := n.conn, m.conn(n.conn), m.clients, m.life
		m.prompt("찾을 글 ", "", func(text string) tea.Cmd {
			if text == "" {
				m.status = "찾을 글이 없어 검색하지 않음"
				return nil
			}
			// 찾는 글은 개인정보(이메일 등)일 수 있어 기록(sql)에 남기지 않는다
			p := pendingRun{conn: conn, readOnly: true, retarget: true, exec: func(ctx context.Context) (*db.Result, error) {
				pg, err := clients.PG(ctx, c)
				if err != nil {
					return nil, err
				}
				res, skipped, err := pg.SearchData(ctx, schema, text)
				if err != nil {
					return nil, err
				}
				note := fmt.Sprintf("데이터 검색 %d건", len(res.Rows))
				if res.Truncated {
					// 결과 요약의 "1000에서 자름" 이 아니라 테이블 수 상한이라 글로 적는다
					note, res.Truncated = note+" · 앞 테이블 200개만 봄", false
				}
				if len(skipped) > 0 {
					note += " · 건너뜀: " + strings.Join(skipped, ", ")
				}
				res.Text = note
				return res, nil
			}}
			// 결과 탭은 prod 확인을 지난 뒤에 고른다. N 이면 빈 탭이 남지 않는다
			open := func() tea.Cmd {
				t := m.cur()
				if conn != t.conn && t.bound() {
					t = m.newTab()
				}
				p.tab = t.id
				return m.guard(p)
			}
			if c.Env() != "prod" {
				return open()
			}
			return withPG(life, clients, c, func(ctx context.Context, pg *db.PGConn, err error) tea.Msg {
				if err != nil {
					return searchCountMsg{conn: conn, err: err, open: open}
				}
				n, err := pg.CountSearchTables(ctx, schema)
				return searchCountMsg{conn: conn, n: n, err: err, open: open}
			})
		})
		return m, nil
	case "N":
		m.openConnForm(n, false)
		return m, nil
	case "E":
		m.openConnForm(n, true)
		return m, nil
	case "alt+X":
		return m, m.removeConnKey(n)
	case "X":
		// PostgreSQL 접속 줄의 X 는 db 전용 접속 지우기다(R59.3). Redis 접속 줄은 아래 패턴 삭제다
		if n != nil && n.kind == nConn && m.conn(n.conn).Kind == db.KindPG {
			return m, m.removeConnAsk(n.conn)
		}
		// ─── Redis 패턴 일괄 삭제 (R42) ───
		// 패턴(이름공간이면 `<이름공간>:*`)을 받아 SCAN 으로 센 뒤 keyScanMsg 에서 미리 보이고 묻는다. PostgreSQL 에서는 하지 않는다.
		if n == nil || (n.kind != nConn && n.kind != nFolder) || m.conn(n.conn).Kind != db.KindRedis {
			m.status = "X(키 일괄 삭제)는 Redis 접속·이름공간 줄에서 누르세요"
			return m, nil
		}
		if m.unlink != nil {
			m.status = "키를 지우는 중 (esc 멈춤)"
			return m, nil
		}
		if m.lockRefuse(n.conn) {
			return m, nil
		}
		pattern := m.patterns[n.conn]
		if n.kind == nFolder {
			pattern = globEscaper.Replace(n.key) + "*"
		}
		conn, c, clients, life := n.conn, m.conn(n.conn), m.clients, m.life
		m.prompt("지울 키 패턴 ", pattern, func(pattern string) tea.Cmd {
			if pattern == "" {
				m.status = "패턴이 없어 지우지 않음"
				return nil
			}
			m.status = "지울 키 세는 중…"
			return func() tea.Msg {
				ctx, cancel := context.WithTimeout(life, loadTimeout)
				defer cancel()
				rc, err := clients.Redis(ctx, c)
				if err != nil {
					return keyScanMsg{conn: conn, pattern: pattern, err: err}
				}
				keys, truncated, err := rc.ScanKeys(ctx, pattern, redisDeleteLimit)
				return keyScanMsg{conn: conn, pattern: pattern, keys: keys, truncated: truncated, err: err}
			}
		})
		return m, nil
	case "I":
		// ─── CSV 가져오기 ───
		// 트리 테이블 줄의 I 는 CSV 경로를 묻는다.
		if n == nil || n.kind != nTable {
			m.status = "I(CSV 가져오기)는 테이블에서 누르세요"
			return m, nil
		}
		m.importNode = n
		m.overlay = oImport
		m.input.Prompt = "CSV 경로 "
		m.input.SetValue(m.proj.Dir + string(filepath.Separator))
		m.input.CursorEnd()
		m.input.Focus()
		return m, m.ensureCatalog(n.conn)
	case "=":
		return m, m.schemaDiff(n)
	case "enter", "l", "right":
		if n == nil {
			break
		}
		return m, m.activate(n, ks == "enter")
	}
	return m, nil
}

func (m *model) pickerMatches() []string {
	f := strings.ToLower(m.input.Value())
	var out []string
	for i, p := range m.pickerItems {
		if f == "" && m.pickerTables[p].quiet {
			continue
		}
		if strings.Contains(m.pickerLower[i], f) {
			out = append(out, p)
		}
	}
	return out
}

func (m *model) historyMatches() []history.Entry {
	if m.hist == nil {
		return nil
	}
	return m.hist.List(history.Filter{Query: m.input.Value(), Limit: historyMax})
}

// ─── View ───

func (m *model) View() string {
	if m.w == 0 {
		return ""
	}
	if m.w < minWidth || m.h < minHeight {
		return fmt.Sprintf("터미널이 너무 작습니다 (%dx%d). 최소 %dx%d.", m.w, m.h, minWidth, minHeight)
	}
	l := m.layout()
	t := m.cur()

	switch m.overlay {
	case oLong:
		return m.longView()
	case oConn:
		return m.connFormView()
	case oPicker:
		list := m.pickerMatches()
		var b strings.Builder
		b.WriteString(m.input.View() + "\n")
		if m.pickDel != nil {
			b.WriteString(dimStyle.Render(m.label("↑↓ 고르기 · Enter 새 탭에 열기 · d(거르기 글이 없을 때)·Ctrl+D 지우기 · esc 닫기")))
		}
		b.WriteString("\n")
		top := max(0, m.pickerCursor-(l.bodyH-3))
		for i := top; i < len(list) && i < top+l.bodyH-2; i++ {
			name := textutil.Printable(list[i], false)
			if tb, ok := m.pickerTables[list[i]]; ok && i != m.pickerCursor {
				// 거르기는 꾸미지 않은 글로 하고 색은 그릴 때만 입힌다
				env := "[" + m.conn(tb.conn).Env() + "]"
				name = envStyle(m.conn(tb.conn)).Render(env) + strings.TrimPrefix(name, env)
			}
			if i == m.pickerCursor {
				name = lipgloss.NewStyle().Reverse(true).Render(name)
			}
			if ci, ok := m.proj.ConnIndex(m.pickerConn[list[i]]); ok {
				name += "  " + envStyle(m.conn(ci)).Render("→ "+textutil.Printable(m.conn(ci).Name, false))
			}
			b.WriteString(truncate(name, m.w) + "\n")
		}
		return b.String()
	case oHistory:
		list := m.historyMatches()
		lines := []string{truncate(m.input.View(), m.w), dimStyle.Render("↑↓ 고르기 · Enter 편집기에 넣기 · esc 닫기")}
		top := max(0, m.histCursor-(m.h-3))
		for i := top; i < len(list) && len(lines) < m.h; i++ {
			e := list[i]
			line := dimStyle.Render(e.Time.Local().Format("01-02 15:04")) + " " + textutil.Printable(e.Conn, false) + "  " + textutil.Printable(oneLine(db.MaskSecrets(e.SQL)), false)
			if e.Err != "" {
				line = errStyle.Render("✗ ") + line
			}
			if i == m.histCursor {
				line = lipgloss.NewStyle().Reverse(true).Render(ansi.Strip(line))
			}
			lines = append(lines, truncate(line, m.w))
		}
		return strings.Join(lines, "\n")
	case oHelp:
		blocks := make([]string, 0, len(helpSections))
		for _, s := range helpSections {
			lines := []string{helpTitle.Render(s.title)}
			for _, kv := range s.keys {
				lines = append(lines, helpKey.Width(22).Render(m.label(kv[0]))+m.label(kv[1]))
			}
			blocks = append(blocks, strings.Join(lines, "\n"))
		}
		// 넓은 화면은 두 단으로 나눠 작은 높이에서도 대부분 보이게 한다
		content := strings.Join(blocks, "\n\n")
		if m.w >= 130 {
			half := (len(blocks) + 1) / 2
			content = lipgloss.JoinHorizontal(lipgloss.Top,
				strings.Join(blocks[:half], "\n\n"), "    ", strings.Join(blocks[half:], "\n\n"))
		}
		lines := strings.Split(content+"\n\n"+dimStyle.Render("아무 키나 눌러 닫기"), "\n")
		if len(lines) > m.h-2 {
			lines = append(lines[:max(1, m.h-3)], dimStyle.Render("… 창을 키우면 더 보입니다"))
		}
		for i, ln := range lines {
			lines[i] = truncate(ln, max(1, m.w-6))
		}
		box := helpBox.Render(strings.Join(lines, "\n"))
		return lipgloss.Place(m.w, m.h, lipgloss.Center, lipgloss.Center, box)
	}

	// ─── 헤더 ───
	header := appBadge.Render(" db ") + barStyle.Render(" ")
	if t.conn >= 0 {
		c := m.conn(t.conn)
		name := c.Name
		if d := m.dbOver[t.conn]; d != "" {
			name += "/" + d
		}
		// F6 으로 고른 스키마(R27.2)는 접속 이름 뒤에 둔다. 세션을 잃어도 db 가 다시 보내므로 지우지 않는다
		if s := m.schemaOf[t.conn]; s != "" {
			name += " · " + s
		}
		header += lipgloss.NewStyle().Background(colBar).Foreground(connColor[c.Kind]).Render(connIcons[c.Kind]) +
			barStyle.Bold(true).Render(textutil.Printable(name, false)) + barStyle.Render(" ") +
			envBadges[c.Env()].Render(" "+strings.ToUpper(c.Env())+" ") +
			hintStyle.Render(" "+connKinds[c.Kind])
		if c.Kind == db.KindPG {
			mode := "Auto"
			if m.manual[t.conn] {
				mode = "Manual"
			}
			if m.inTx[t.conn] {
				d := time.Since(m.txSince[t.conn]).Round(time.Second)
				header += txBadge.Render(fmt.Sprintf("  %s · tx %02d:%02d", mode, int(d.Minutes()), int(d.Seconds())%60))
			} else {
				header += hintStyle.Render("  " + mode)
			}
		}
		if b := m.lockBadge(t.conn); b != "" {
			header += txBadge.Render("  " + b)
		}
	} else {
		header += hintStyle.Render("접속 없음 · 트리에서 고르세요")
	}
	header = m.bar(header, hintStyle.Render(textutil.Printable(filepath.Base(m.proj.Dir), false)+" "))

	// ─── 트리 ───
	if m.cursor < m.treeTop {
		m.treeTop = max(0, m.cursor)
	}
	if m.cursor >= m.treeTop+l.treeH {
		m.treeTop = m.cursor - l.treeH + 1
	}
	var tb strings.Builder
	for i := m.treeTop; i < len(m.rows) && i < m.treeTop+l.treeH; i++ {
		r := m.rows[i]
		n := r.n
		toggle := "  "
		if n.expandable() {
			toggle = "▸ "
			if n.expanded {
				toggle = "▾ "
			}
		}
		icon, iconSt := kindIcons[n.kind], dimStyle
		label := textutil.Printable(n.label, false)
		style := lipgloss.NewStyle().Foreground(colText)
		switch n.kind {
		case nGroup:
			style = style.Bold(true)
		case nConn:
			c := m.conn(n.conn)
			icon, iconSt = connIcons[c.Kind], lipgloss.NewStyle().Foreground(connColor[c.Kind])
			style = envStyle(c)
			if d := m.dbOver[n.conn]; d != "" {
				label += "/" + textutil.Printable(d, false)
			}
			if c.Source == db.SourceDB {
				label += " (db)" // R59.1
			}
			if n.conn == t.conn {
				label = "● " + label
			}
		case nInfo:
			style = dimStyle
		}
		if i == m.cursor {
			// 포커스가 없을 때도 선택 줄을 옅게 보여 줘야 마우스로 고른 줄을 놓치지 않는다
			sel := lipgloss.NewStyle().Background(colBorder)
			if m.focus == fTree {
				sel = lipgloss.NewStyle().Reverse(true)
			}
			style, iconSt = style.Inherit(sel), iconSt.Inherit(sel)
		}
		indent := strings.Repeat("  ", r.depth)
		line := style.Render(indent+toggle) + iconSt.Render(icon) + style.Render(label)
		tb.WriteString(truncate(line, l.treeW) + "\n")
	}
	treeInfo := ""
	if m.filter != "" {
		treeInfo = "거르기 " + textutil.Printable(m.filter, false)
	}

	// ─── 탭 줄 ───
	labels := m.tabLabels()
	tabs := make([]string, 0, len(labels)+1)
	for i, lb := range labels {
		st := tabInactive
		if i == m.tab {
			st = tabActive
		}
		tabs = append(tabs, st.Render(lb))
	}
	tabs = append(tabs, tabInactive.Render(tabPlus))
	tabBar := truncate(strings.Join(tabs, " "), l.rightW+2)

	// ─── 편집기 ───
	edBody := t.editor.View()
	if t.editor.Value() == "" && l.rightW > placeholderX {
		first, rest, _ := strings.Cut(edBody, "\n")
		edBody = ansi.Truncate(first, placeholderX, "") + dimStyle.Render(truncate(" "+m.label(placeholder), l.rightW-placeholderX)) + "\n" + rest
	}
	edInfo := ""
	if t.file != "" {
		edInfo = textutil.Printable(t.file, false)
	}

	// ─── 결과 ───
	var rb string
	lines := m.resultLines(t, l.rightW)
	switch {
	case m.overlay == oConfirm && m.yes != nil && len(m.yes.preview) > 0:
		pl := []string{dimStyle.Render("y 실행 · 그 밖의 키 취소")}
		for _, ln := range m.yes.preview {
			if len(pl) >= l.resH {
				break
			}
			pl = append(pl, truncate(textutil.Printable(ln, false), l.rightW))
		}
		rb = strings.Join(pl, "\n")
	case m.overlay == oConfirm && m.pending != nil && len(m.pending.preview) > 0:
		p := m.pending
		pl := []string{dimStyle.Render(fmt.Sprintf("미리 보기 %d줄 · ↑↓ 넘기기 · y 실행 · 그 밖의 키 취소", len(p.preview)))}
		for i := m.previewTop; i < len(p.preview) && len(pl) < l.resH; i++ {
			pl = append(pl, truncate(textutil.Printable(p.preview[i], false), l.rightW))
		}
		rb = strings.Join(pl, "\n")
	case t.redis != nil:
		*t.redis = t.redis.SetSize(l.rightW, l.resH)
		rb = t.redis.View()
	case t.planOn:
		t.plan.SetSize(l.rightW, l.resH)
		rb = t.plan.View()
	case t.resErr != "":
		rb = strings.Join(append(lines, errStyle.Width(l.rightW).Render(textutil.Printable(t.resErr, true))), "\n")
	case t.shown() == nil:
		rb = dimStyle.Render("결과가 여기에 나옵니다")
	default:
		// 표 위 줄(결과 탭·테이블 보기) 수가 바뀔 때만 크기를 다시 맞춘다. 맞출 때마다 칸 폭을 다시 재서다
		if h := max(1, l.resH-len(lines)); t.gridH != h {
			t.grid.SetSize(l.rightW, h)
			t.gridH = h
		}
		rb = strings.Join(append(lines, t.grid.View(m.focus == fResult)), "\n")
	}
	resInfo := textutil.Printable(t.summary, false)
	if t.running != nil {
		resInfo = "실행 중…"
	}

	body := lipgloss.JoinHorizontal(lipgloss.Top,
		pane(paneSpec{title: "Explorer", info: treeInfo, body: strings.TrimRight(tb.String(), "\n"), w: l.treeW, h: l.treeH, focused: m.focus == fTree,
			hot: m.drag == bTree}),
		lipgloss.JoinVertical(lipgloss.Left,
			tabBar,
			pane(paneSpec{title: textutil.Printable(t.displayName(), false), info: edInfo, body: edBody, w: l.rightW, h: l.edH, focused: m.focus == fEditor,
				hot: m.drag == bSplit}),
			pane(paneSpec{title: "Result", info: resInfo, body: rb, w: l.rightW, h: l.resH, focused: m.focus == fResult})))

	switch m.overlay {
	case oFilter:
		return header + "\n" + body + "\n" + truncate(m.input.View(), m.w)
	case oParams, oImport, oPrompt:
		return header + "\n" + body + "\n" + truncate(m.input.View(), m.w)
	case oConfirm:
		// prod 는 쓰기·제출 모두 접속 이름을 앞에 붙여 한 번의 확인으로 알아보게 한다
		p := m.pending
		if m.yes != nil {
			return header + "\n" + body + "\n" + truncate(confirmBar.Render(textutil.Printable(" "+m.yes.q+": y/N ", false)), m.w)
		}
		name := m.conn(p.conn).Name
		est := ""
		if p.estimate != "" {
			est = " · " + p.estimate
		}
		var q string
		switch a := m.askOf(p); {
		case a == askDiscard:
			q = fmt.Sprintf(" 변경 %d건을 버릴까요 y/N ", m.findTab(p.tab).grid.Pending())
		case a == askAnalyze:
			q = " EXPLAIN ANALYZE 는 문장을 실제로 실행합니다: y/N "
		case p.danger != "" && p.confirm == "" && m.conn(p.conn).Env() == "prod" && !p.readOnly:
			q = fmt.Sprintf(" prod(%s)에 쓰기 실행 · 위험: %s%s — 실행할까요? y/N ", name, p.danger, est)
		case p.danger != "" && p.confirm == "":
			q = fmt.Sprintf(" 위험: %s%s — 실행할까요? y/N ", p.danger, est)
		case p.confirm != "" && m.conn(p.conn).Env() == "prod":
			q = fmt.Sprintf(" prod(%s)에 %s: y/N ", name, p.confirm)
		case p.confirm != "":
			q = " " + p.confirm + ": y/N "
		default:
			q = fmt.Sprintf(" prod(%s)에 쓰기 실행%s: y/N ", name, est)
		}
		return header + "\n" + body + "\n" + truncate(confirmBar.Render(textutil.Printable(q, false)), m.w)
	case oTx:
		var names []string
		label := "열린 트랜잭션"
		for _, c := range m.txAsk.conns {
			names = append(names, m.conn(c).Name)
			if m.conn(c).Env() == "prod" && !strings.HasSuffix(label, ")") {
				label += "(prod 포함)"
			}
		}
		q := fmt.Sprintf(" %s %s: c=Commit r=Rollback esc=취소 ", label, strings.Join(names, ", "))
		return header + "\n" + body + "\n" + truncate(confirmBar.Render(textutil.Printable(q, false)), m.w)
	case oFinder:
		return lipgloss.Place(m.w, m.h, lipgloss.Center, lipgloss.Center, m.finder.View())
	}

	// ─── 상태 줄 ───
	status := barStyle.Render(" " + textutil.Printable(m.status, false))
	return header + "\n" + body + "\n" + m.bar(status, hintStyle.Render(m.label(focusHints[m.focus])+" "))
}

// bar 는 헤더·상태 줄처럼 왼쪽과 오른쪽 글을 화면 폭에 맞춰 배경색으로 채운 한 줄이다.
// 좁으면 오른쪽(키 도움말 등)을 버리고 왼쪽을 자른다.
func (m *model) bar(left, right string) string {
	gap := m.w - lipgloss.Width(left) - lipgloss.Width(right)
	if gap < 1 {
		left = truncate(left, m.w)
		return left + barStyle.Render(strings.Repeat(" ", max(0, m.w-lipgloss.Width(left))))
	}
	return left + barStyle.Render(strings.Repeat(" ", gap)) + right
}

func oneLine(s string) string {
	s = strings.ReplaceAll(s, "\n", " ")
	if r := []rune(s); len(r) > 200 {
		return string(r[:200]) + "…"
	}
	return s
}

// errText 는 접속 conn 의 오류를 상태 줄 한 줄로 만든다. 오류 글에 섞인 비밀번호를 지운다.
func (m *model) errText(conn int, err error) string {
	if errors.Is(err, db.ErrExportNotSelect) {
		return "SELECT 문만 전체 내보내기"
	}
	return oneLine(db.Redact(err.Error(), m.conn(conn).Password))
}

// withPG 는 명령 고루틴에서 loadTimeout 안에 접속 c 의 PostgreSQL 연결을 얻어 fn 에 넘긴다.
// 연결 오류도 fn 에 넘겨 부르는 쪽이 자기 메시지로 돌려주게 한다.
func withPG(life context.Context, clients *db.Clients, c db.Conn, fn func(ctx context.Context, pg *db.PGConn, err error) tea.Msg) tea.Cmd {
	return func() tea.Msg {
		ctx, cancel := context.WithTimeout(life, loadTimeout)
		defer cancel()
		pg, err := clients.PG(ctx, c)
		return fn(ctx, pg, err)
	}
}

// label 은 키 안내를 맥 글자(⌘)로 바꾼다(R47.1). `Ctrl+R / F5` 처럼 같은 ⌘ 키가 되는 대안은 하나로 줄인다.
// 사용자 값(탭 제목·오류·DB 값)이 섞인 글에는 쓰지 않는다. F5 같은 낱말까지 키로 읽어 바꾸기 때문이다.
// 값을 넣는 글은 형식 문자열에만 쓰고 값은 그 뒤 fmt.Sprintf 로 넣는다.
func (m *model) label(s string) string {
	if !m.mac {
		return s
	}
	return strings.Join(slices.Compact(strings.Split(keys.Label(s, true), " / ")), " / ")
}

func truncate(s string, w int) string {
	return ansi.Truncate(s, w, "…")
}

// clamp 는 길이 n 인 목록의 인덱스로 맞춘다. 빈 목록이면 0.
func clamp(i, n int) int {
	return max(0, min(i, n-1))
}
