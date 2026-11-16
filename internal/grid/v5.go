package grid

// ─── 다음 쿼리로 잇기·복호화 보기 (v5) ───
// C·F·R·+·- 는 다음 쿼리의 조건이 될 값을 다루므로 화면 값이 아니라 DB 에 있는 원래 값(보류 중인 바꿈 전)을 쓴다.
// 복호화한 값은 화면·y·Y·값 보기에만 쓰인다.

import (
	"fmt"
	"math"
	"slices"
	"strconv"
	"strings"

	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

const maxInItems = 1000 // 붙여 넣은 쿼리가 편집기·서버에서 다루기 버거워지지 않을 만큼

// Follow 는 F·R 로 외래 키를 따라가 달라는 요청이다(R36). FK 를 찾고 WHERE 를 만드는 일은 통합이 한다.
// Col 은 커서 칸의 테이블 열 이름(식 열이면 "")이고, Row 는 커서 행의 테이블 열 → DB 값이다.
// 여러 열 FK 도 Row 에서 모든 열 값을 꺼낼 수 있다. db.Result 처럼 NULL 은 "NULL" 글자다.
// Reverse 면 R(이 행을 참조하는 테이블), 아니면 F(이 칸이 참조하는 테이블)다.
type Follow struct {
	Col     string
	Row     map[string]string
	Reverse bool
}

// Linked 는 Ctrl+B 로 커서 칸 값이 든 다른 테이블을 찾아 달라는 요청이다(R73). 찾기는 통합이 한다.
// Col 은 칸 열 이름(원본을 알면 테이블 열 이름), Table 은 원본 테이블 이름(모르면 "")이고 Value 는 DB 값이다.
type Linked struct {
	Col, Table, Value string
}

// CellFilter 는 +·- 로 커서 칸 값을 서버 WHERE 에 더하거나 빼 달라는 요청이다(R38).
// 조건 글은 통합이 tableview.Condition 으로 만든다. Null 이면 Value 는 쓰지 않는다.
type CellFilter struct {
	Col    string
	Value  string
	Null   bool
	Negate bool
}

// cellKey 는 결과 행 인덱스와 열이다.
type cellKey struct{ row, col int }

// plainCell 은 푼 값이다. raw 는 풀 때의 원래 값으로, 칸 값이 그새 바뀌었으면(편집·제출) 푼 값을 쓰지 않는다.
type plainCell struct{ raw, text string }

// SetDecrypted 는 looks 가 참인 결과 칸을 decrypt 로 풀어 보인다(R37). 풀지 못한 칸 수를 돌려주고 그 칸은 원래 값 그대로다.
// 거르기·스크롤마다 다시 풀지 않도록 결과 전체(최대 db.MaxRows 행)를 한 번에 푼다. 추가 행은 풀지 않는다.
// 푼 값은 화면·y·Y·값 보기에만 쓰고 내보내기·SQL 복사·IN 목록·비교·차트·거르기(/)·정렬·제출은 원래 값을 쓴다.
// 복호화 보기 중에는 칸 편집을 막는다(cellBlock). 새 결과(SetResult)를 받으면 꺼진다.
func (m *Model) SetDecrypted(looks func(string) bool, decrypt func(string) (string, error)) (failed int) {
	m.dec, m.decOn = map[cellKey]plainCell{}, true
	if m.res == nil {
		return 0
	}
	// 푼 값(한글 등)이 원래 값보다 넓을 수 있어 열 폭을 늘린다. 끌 때 줄이지는 않는다
	m.widths = slices.Clone(m.widths)
	for i, row := range m.res.Rows {
		for c, v := range row {
			if v == nullText || !looks(v) {
				continue
			}
			p, err := decrypt(v)
			if err != nil {
				failed++
				continue
			}
			m.dec[cellKey{i, c}] = plainCell{raw: v, text: p}
			if c < len(m.widths) {
				m.widths[c] = max(m.widths[c], min(autoMaxWidth, ansi.StringWidth(cellText(p, autoMaxWidth))))
			}
		}
	}
	return failed
}

// ClearDecrypted 는 복호화 보기를 끄고 원래 값으로 보인다.
func (m *Model) ClearDecrypted() {
	m.dec, m.decOn = nil, false
}

// Decrypted 는 복호화 보기 중인지다. D 를 받은 통합이 켤지 끌지 고른다.
func (m Model) Decrypted() bool {
	return m.decOn
}

// plain 은 결과 행 i 열 c 의 지금 값 v 를 화면에 보일 값으로 바꾼다. 푼 값을 보이면 참이다.
func (m Model) plain(i, c int, v string) (string, bool) {
	p, ok := m.dec[cellKey{i, c}]
	if !ok || p.raw != v {
		return v, false
	}
	return p.text, true
}

// shown 은 화면에 보이는 행 값이다(보류 중인 바꿈 + 푼 값). 복사·값 보기가 쓴다.
func (m Model) shown(i int) []string {
	row := m.current(i)
	if len(m.dec) == 0 {
		return row
	}
	out := slices.Clone(row)
	for c, v := range out {
		out[c], _ = m.plain(i, c, v)
	}
	return out
}

// inList 는 view 행 r0..r1, 열 c0..c1 칸의 DB 값을 IN 목록으로 복사한다(R35). 보이는 순서대로 같은 값은 한 번만 넣고
// NULL·추가 행은 뺀다. 타입이 있는 숫자 열은 유한한 숫자로 읽히면 그대로, 아니면(NaN·Infinity 등) 리터럴로 감싼다.
// 타입이 없는 결과(Redis)는 값으로 숫자 열을 추정하므로 표준 숫자 꼴(canonicalNumber)일 때만 그대로 둔다.
func (m Model) inList(r0, r1, c0, c1 int) Action {
	seen := map[string]bool{}
	var items []string
	capped := false
	for vi := r0; vi <= r1 && !capped; vi++ {
		i := m.view[vi]
		if i >= len(m.res.Rows) {
			continue
		}
		for c := c0; c <= c1; c++ {
			v := cellOf(m.res.Rows[i], c)
			if v == nullText {
				continue
			}
			lit := db.QuoteLiteral(v)
			if m.kinds[c] == kindNumber {
				if f, err := strconv.ParseFloat(v, 64); err == nil && !math.IsNaN(f) && !math.IsInf(f, 0) && (typed(m.res) || canonicalNumber(v)) {
					lit = v
				}
			}
			if seen[lit] {
				continue
			}
			if len(items) == maxInItems {
				capped = true
				break
			}
			seen[lit] = true
			items = append(items, lit)
		}
	}
	if len(items) == 0 {
		return Action{Status: "복사할 값이 없음"}
	}
	text := "(" + strings.Join(items, ", ") + ")"
	status := fmt.Sprintf("IN 목록 %d개 복사", len(items))
	if capped {
		status = fmt.Sprintf("앞 %s개만 복사", textutil.GroupDigits(maxInItems))
	}
	return Action{Copy: &text, Status: status}
}

// canonicalNumber 는 v 가 숫자로 읽고 다시 쓴 꼴과 같은지다. "007"·"+5"·"1e3"·"1.50"·"-0" 처럼 숫자로 바꾸면
// 글자가 달라지는 값은 아니다. 타입 없는 값(Redis 의 "007" 같은 문자열 id)을 숫자로 바꿔 다른 행을 찾지 않게 한다.
func canonicalNumber(v string) bool {
	if !strings.Contains(v, ".") {
		n, err := strconv.ParseInt(v, 10, 64)
		return err == nil && strconv.FormatInt(n, 10) == v
	}
	f, err := strconv.ParseFloat(v, 64)
	return err == nil && strconv.FormatFloat(f, 'f', -1, 64) == v
}
