package db

import (
	"slices"
	"strings"
)

// DangerOf 는 환경과 상관없이 확인을 받아야 할 문장이면 이유를, 아니면 빈 문자열을 돌려준다(명세 R25).
// 문장이 여럿이면 문장마다 보고 서로 다른 이유를 ", " 로 잇는다.
// WHERE 는 괄호 밖 최상위의 것만 센다. 하위 쿼리 안의 where 는 지울 행을 좁히지 않아서다.
// 단, with … delete(CTE 안 쓰기)·함수 안 쓰기·explain analyze delete 는 첫 단어로 보므로 잡지 못한다(R6.1 과 같은 한계).
func DangerOf(stmt string) string {
	var reasons []string
	for _, words := range topLevelWords(stmt) {
		var r string
		switch words.first {
		case "delete", "update":
			if !slices.Contains(words.top, "where") {
				r = "WHERE 없는 " + strings.ToUpper(words.first)
			}
		case "truncate":
			r = "TRUNCATE"
		case "drop":
			r = "DROP"
		case "alter":
			// ALTER [COLUMN] c DROP NOT NULL·DEFAULT·IDENTITY·EXPRESSION 은 제약·기본값만 푼다. 열·제약을 지우는 DROP 만 잡는다.
			// 열 하위 절(ALTER [COLUMN] c)이나 ALTER DOMAIN d 바로 뒤일 때만 봐준다. ALTER TABLE t DROP identity 는 COLUMN 을 뺀 열 지우기라서다
			for i, w := range words.top {
				inColumn := i >= 2 && words.top[i-2] == "alter" || i >= 3 && words.top[i-3] == "alter" && words.top[i-2] == "column" ||
					i == 3 && words.top[1] == "domain"
				if w == "drop" && (i+1 == len(words.top) || !inColumn || !alterDropKeeps[words.top[i+1]]) {
					r = "DROP"
					break
				}
			}
		}
		if r != "" && !slices.Contains(reasons, r) {
			reasons = append(reasons, r)
		}
	}
	return strings.Join(reasons, ", ")
}

// HiddenWriteOf 는 첫 단어 판정(DangerOf·IsReadOnlySQL)이 놓치는 쓰기·부수효과의 이유를 ", " 로 이어 돌려준다.
// CLI query --write(R58)가 --force 를 요구하는 데 쓴다. CTE 는 괄호 깊이와 상관없이 쓰기 단어를 찾는다.
// 한계: 함수 안의 쓰기와 이름을 모르는 부수효과 함수는 잡지 못한다.
func HiddenWriteOf(stmt string) string {
	var reasons []string
	for _, w := range topLevelWords(stmt) {
		if w.first == "with" && hasDataWrite(w.all) {
			reasons = append(reasons, "CTE 안 쓰기")
		}
		if (w.first == "select" || w.first == "with") && mainVerb(w.top) == "select" && slices.Contains(w.top, "into") {
			reasons = append(reasons, "SELECT INTO")
		}
	}
	switch name, err := blockedCall(stmt); {
	case err != nil:
		reasons = append(reasons, `U&"…" 식별자`)
	case name != "":
		reasons = append(reasons, "부수효과 함수 "+name)
	}
	// 문장이 여럿이면 같은 이유가 겹친다
	slices.Sort(reasons)
	reasons = slices.Compact(reasons)
	return strings.Join(reasons, ", ")
}

// hasDataWrite 는 단어들에 insert·update·delete·merge 가 있는지 본다. select … for [no key] update 의 update 는 잠금이라 뺀다.
func hasDataWrite(words []string) bool {
	for i, w := range words {
		switch w {
		case "insert", "delete", "merge":
			return true
		case "update":
			if i == 0 || words[i-1] != "for" && words[i-1] != "key" {
				return true
			}
		}
	}
	return false
}

// mainVerb 는 with 절 뒤 본 문장의 동사다(괄호 밖 첫 select·insert·update·delete·merge).
func mainVerb(top []string) string {
	i := slices.IndexFunc(top, func(w string) bool {
		return w == "select" || w == "insert" || w == "update" || w == "delete" || w == "merge"
	})
	if i < 0 {
		return ""
	}
	return top[i]
}

// alterDropKeeps 는 ALTER 의 DROP 뒤에 오면 데이터를 지우지 않는 단어다.
var alterDropKeeps = map[string]bool{"not": true, "default": true, "identity": true, "expression": true}

// DangerOfRedis 는 FLUSHDB·FLUSHALL 이면 그 이름을 돌려준다.
func DangerOfRedis(args []string) string {
	if len(args) == 0 {
		return ""
	}
	if cmd := strings.ToUpper(args[0]); cmd == "FLUSHDB" || cmd == "FLUSHALL" {
		return cmd
	}
	return ""
}

// stmtWords 는 문장 하나의 첫 단어(괄호 깊이와 상관없이)와 괄호 밖 단어들, 모든 단어다. 모두 소문자다.
type stmtWords struct {
	first    string
	top, all []string
}

// topLevelWords 는 MaskSQL 로 문자열·주석·"따옴표 이름"을 지운 뒤 괄호 밖 ';' 마다 문장을 나눠 단어를 모은다.
func topLevelWords(text string) []stmtWords {
	m := MaskSQL(text)
	var out []stmtWords
	cur := stmtWords{}
	depth := 0
	for i := 0; i < len(m); i++ {
		c := m[i]
		switch {
		case c == '(':
			depth++
		case c == ')':
			depth = max(depth-1, 0)
		case c == ';' && depth == 0:
			out = append(out, cur)
			cur = stmtWords{}
		case IsIdentByte(c) && c != '$' && (c < '0' || c > '9'):
			j := i
			for j < len(m) && IsIdentByte(m[j]) {
				j++
			}
			w := strings.ToLower(m[i:j])
			if cur.first == "" {
				cur.first = w
			}
			if depth == 0 {
				cur.top = append(cur.top, w)
			}
			cur.all = append(cur.all, w)
			i = j - 1
		case IsIdentByte(c): // 숫자로 시작하는 토큰(1e5 등)은 단어가 아니다
			for i+1 < len(m) && IsIdentByte(m[i+1]) {
				i++
			}
		}
	}
	return append(out, cur)
}
