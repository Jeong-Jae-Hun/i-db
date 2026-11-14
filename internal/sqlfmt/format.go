package sqlfmt

import "strings"

const (
	indentUnit = "    "
	// 대문자로 바꿀 낱말이다. 열 이름으로 흔한 낱말(name·type·key·value·first 등)은 넣지 않는다
	keywordList = `
all and any array as asc between by case cast check collate conflict constraint create cross current_date current_time
current_timestamp current_user default delete desc distinct do drop else end except exists false fetch filter for
foreign from full group having ilike in inner insert intersect interval into is join lateral left like limit natural
not nothing null nulls offset on only or order outer over partition primary recursive references returning right
select set similar some table then true union unique update using values when where window with`
)

var (
	keywords = map[string]bool{}

	// 이 낱말 뒤의 JOIN 은 이미 줄을 바꾼 조인 구의 일부다
	joinMods = map[string]bool{"left": true, "right": true, "full": true, "inner": true, "cross": true, "natural": true, "outer": true}

	// 이 절 안의 최상위 쉼표 뒤에서 줄을 바꾼다
	commaClauses = map[string]bool{"select": true, "values": true, "set": true}

	clauseWords = map[string]bool{
		"select": true, "from": true, "where": true, "group": true, "having": true, "order": true, "limit": true,
		"offset": true, "returning": true, "values": true, "set": true, "insert": true, "update": true, "delete": true,
		"union": true, "intersect": true, "except": true,
	}
)

// frame 은 괄호 한 겹이다. stmt 는 최상위나 서브쿼리 괄호로 절마다 줄을 바꾸는 틀이다.
type frame struct {
	stmt    bool
	started bool
	indent  int // 이 틀의 절이 놓일 들여쓰기 단계
	open    int // 닫는 괄호가 놓일 들여쓰기 단계
	clause  string
	between bool // BETWEEN 뒤 첫 AND 는 조건 연결이 아니다
	cases   int  // CASE 안에서는 AND·OR·쉼표로 줄을 바꾸지 않는다
}

func init() {
	for _, w := range strings.Fields(keywordList) {
		keywords[w] = true
	}
}

// Format 은 절 앞에서 줄을 바꾸고 키워드를 대문자로 쓴다. 문자열·따옴표 이름·주석은 그대로 두고,
// 줄바꿈 규칙이 없는 자리의 공백은 원래 공백이 있었으면 한 칸, 없었으면 붙여 쓴다.
// 줄바꿈은 토큰만 보고 정하므로 결과를 다시 넣어도 같다(멱등).
func Format(sql string) string {
	toks := Tokenize(sql)
	var b strings.Builder
	stack := []frame{{stmt: true}}
	line := 0
	prev := ""
	afterSemi, opened, breakComma := false, false, false
	for i, t := range toks {
		w := norm(toks, i)
		next := nextSig(toks, i)
		f := &stack[len(stack)-1]
		brk := false
		if f.stmt && f.started {
			switch w {
			case "select", "where", "having", "limit", "offset", "returning", "union", "intersect", "except", "insert",
				"inner", "cross", "natural":
				brk = true
			case "from":
				// DELETE FROM, IS DISTINCT FROM 은 한 구다
				brk = prev != "delete" && prev != "distinct"
			case "group", "order":
				brk = next == "by"
			case "update", "delete":
				// ON CONFLICT DO UPDATE, FOR UPDATE, ON DELETE CASCADE 는 절이 아니다
				brk = prev != "do" && prev != "for" && prev != "on"
			case "values":
				brk = prev != "default"
			case "set":
				brk = f.clause == "update"
			case "left", "right", "full":
				brk = next == "join" || next == "outer"
			case "join":
				brk = !joinMods[prev]
			}
		}
		cont := f.stmt && f.cases == 0 && (breakComma ||
			(w == "and" && !f.between || w == "or") && (f.clause == "where" || f.clause == "having"))

		sep, nl := "", -1
		switch {
		case i == 0:
		case afterSemi:
			sep, line = "\n\n", 0
		case w == ")" && len(stack) > 1 && f.stmt:
			nl = f.open
		case opened:
			nl = f.indent
		case brk:
			nl = f.indent
		case cont:
			nl = f.indent + 1
		case toks[i-1].Kind == LineComment:
			nl = line
		case w == ";":
		case toks[i-1].End < t.Start:
			sep = " "
		}
		if nl >= 0 {
			sep, line = "\n"+strings.Repeat(indentUnit, nl), nl
		}
		b.WriteString(sep)
		if t.Kind == Word && keywords[w] {
			b.WriteString(strings.ToUpper(t.Text))
		} else {
			b.WriteString(t.Text)
		}

		afterSemi, opened, breakComma = false, false, false
		if t.Kind == LineComment || t.Kind == BlockComment {
			continue
		}
		f.started = true
		if f.stmt && clauseWords[w] && (brk || w != "set") {
			f.clause = w
		}
		switch w {
		case "between":
			f.between = true
		case "and":
			f.between = false
		case "case":
			f.cases++
		case "end":
			f.cases = max(0, f.cases-1)
		case ",":
			breakComma = f.stmt && f.cases == 0 && commaClauses[f.clause]
		case "(":
			if next == "select" || next == "with" || next == "values" {
				stack = append(stack, frame{stmt: true, indent: line + 1, open: line})
				opened = true
			} else {
				stack = append(stack, frame{})
			}
		case ")":
			if len(stack) > 1 {
				stack = stack[:len(stack)-1]
			}
		case ";":
			// db.SplitStatements 처럼 괄호와 상관없이 ';' 에서 문장을 끊는다
			stack, line, afterSemi = []frame{{stmt: true}}, 0, true
		}
		prev = w
	}
	return b.String()
}

// norm 은 규칙 비교용 낱말이다. 기호는 그대로, 낱말은 소문자로, t.col 처럼 점에 붙은 이름과 그 밖의 토큰은 빈 문자열이다.
func norm(toks []Token, i int) string {
	t := toks[i]
	switch t.Kind {
	case Punct:
		return t.Text
	case Word:
		if i > 0 && toks[i-1].Text == "." || i+1 < len(toks) && toks[i+1].Text == "." {
			return ""
		}
		return strings.ToLower(t.Text)
	}
	return ""
}

// nextSig 는 i 다음의 주석이 아닌 토큰의 norm 이다.
func nextSig(toks []Token, i int) string {
	for j := i + 1; j < len(toks); j++ {
		if k := toks[j].Kind; k != LineComment && k != BlockComment {
			return norm(toks, j)
		}
	}
	return ""
}
