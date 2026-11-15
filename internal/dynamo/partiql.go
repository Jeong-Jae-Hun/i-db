package dynamo

import "strings"

// ─── PartiQL 낱말 ───
//
// 쓰기 판정과 파티션 키 조건 찾기에 필요한 만큼만 자른다. 문법 검사는 DynamoDB 가 한다.
// 주석(-- …, /* … */)은 버리고, 문자열('…')·따옴표 이름("…") 안의 글자는 낱말로 보지 않는다.

type tokKind int

const (
	tokWord   tokKind = iota // 따옴표 없는 이름·예약어
	tokIdent                 // "이름"
	tokString                // '문자열'
	tokNumber
	tokPunct
)

type token struct {
	kind tokKind
	text string // tokIdent·tokString 은 따옴표를 벗기고 겹따옴표('' "")를 푼 값
}

func tokenize(s string) []token {
	var out []token
	for i := 0; i < len(s); {
		c := s[i]
		switch {
		case c == ' ' || c == '\t' || c == '\n' || c == '\r':
			i++
		case c == '-' && i+1 < len(s) && s[i+1] == '-':
			for i < len(s) && s[i] != '\n' {
				i++
			}
		case c == '/' && i+1 < len(s) && s[i+1] == '*':
			end := strings.Index(s[i+2:], "*/")
			if end < 0 {
				i = len(s)
			} else {
				i += 2 + end + 2
			}
		case c == '\'' || c == '"':
			var b strings.Builder
			j := i + 1
			for j < len(s) {
				if s[j] == c {
					if j+1 < len(s) && s[j+1] == c {
						b.WriteByte(c)
						j += 2
						continue
					}
					j++
					break
				}
				b.WriteByte(s[j])
				j++
			}
			kind := tokString
			if c == '"' {
				kind = tokIdent
			}
			out = append(out, token{kind, b.String()})
			i = j
		case isWordByte(c):
			j := i
			for j < len(s) && isWordByte(s[j]) {
				j++
			}
			kind := tokWord
			if c >= '0' && c <= '9' {
				kind = tokNumber
			}
			out = append(out, token{kind, s[i:j]})
			i = j
		default:
			out = append(out, token{tokPunct, s[i : i+1]})
			i++
		}
	}
	return out
}

func isWordByte(c byte) bool {
	return c == '_' || c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || c >= 0x80
}

func (t token) is(word string) bool { return t.kind == tokWord && strings.EqualFold(t.text, word) }

// isName 은 이름 자리(따옴표 유무 무관) 낱말인지 본다.
func (t token) isName() bool { return t.kind == tokWord || t.kind == tokIdent }

// ─── 판정 ───

// isSelect 는 문장 하나짜리 SELECT 인지 본다. 끝의 ; 하나는 받되 그 뒤에 다른 문장이 있으면 아니다
// (ExecuteStatement 가 한 문장만 받지만 보내기 전에 막는다).
func isSelect(toks []token) bool {
	if len(toks) == 0 || !toks[0].is("SELECT") {
		return false
	}
	for i, t := range toks {
		if t.kind == tokPunct && t.text == ";" {
			return i == len(toks)-1
		}
	}
	return true
}

// target 은 FROM 뒤 테이블과 인덱스 이름이다("t"."idx"). 찾지 못하면 ok 가 false 다.
func target(toks []token) (table, index string, ok bool) {
	i := topLevel(toks, 0, "FROM")
	if i < 0 || i+1 >= len(toks) || !toks[i+1].isName() {
		return "", "", false
	}
	table = toks[i+1].text
	if i+3 < len(toks) && toks[i+2].kind == tokPunct && toks[i+2].text == "." && toks[i+3].isName() {
		index = toks[i+3].text
	}
	return table, index, true
}

// hasKeyCondition 은 WHERE 가 파티션 키를 = 또는 IN 으로 고정하는지 본다. 괄호로 감싼 조건은 벗겨서 다시 본다.
// 판단이 서지 않으면 false(확인받기) 쪽으로 둔다.
//
// OR 로 갈린 조건은 가지마다 키가 있어도(`pk = 'a' OR pk = 'b'`) 스캔으로 본다. AWS 문서는 스캔을 피하려면
// 파티션 키를 = 또는 IN 으로 지정하라고만 하고 OR 가 Query 로 바뀌는지는 적지 않는다. 여러 값은 IN 으로 쓴다.
func hasKeyCondition(toks []token, pk string) bool {
	w := topLevel(toks, 0, "WHERE")
	if w < 0 {
		return false
	}
	end := len(toks)
	for _, stop := range []string{"ORDER", "LIMIT"} {
		if j := topLevel(toks, w+1, stop); j >= 0 && j < end {
			end = j
		}
	}
	if end > 0 && toks[end-1].kind == tokPunct && toks[end-1].text == ";" {
		end--
	}
	return branchHasKey(toks[w+1:end], pk)
}

func branchHasKey(toks []token, pk string) bool {
	if len(splitTopLevel(toks, "OR")) > 1 {
		return false
	}
	if wrapped(toks) {
		return branchHasKey(toks[1:len(toks)-1], pk)
	}
	// AND 로 묶인 조각 가운데 하나라도 키를 고정하면 된다. (pk = 'a') AND … 처럼 괄호 친 조각은 벗겨 본다
	for _, part := range splitTopLevel(toks, "AND") {
		if wrapped(part) && branchHasKey(part[1:len(part)-1], pk) || conjunctionHasKey(part, pk) {
			return true
		}
	}
	return false
}

// conjunctionHasKey 는 AND·OR 가 없는 조각에서 맨 바깥의 `pk = 값`·`값 = pk`·`pk IN …` 을 찾는다.
func conjunctionHasKey(toks []token, pk string) bool {
	depth := 0
	for i, t := range toks {
		if t.kind == tokPunct {
			switch t.text {
			case "(", "[":
				depth++
			case ")", "]":
				depth--
			}
		}
		if depth != 0 || !t.isName() || t.text != pk || t.kind == tokWord && isKeyword(t.text) {
			continue
		}
		// a.pk 처럼 중첩 경로의 일부거나 NOT pk = … 은 키 조건이 아니다
		if i > 0 && (toks[i-1].kind == tokPunct && toks[i-1].text == "." || toks[i-1].is("NOT")) {
			continue
		}
		if i+1 < len(toks) && (toks[i+1].kind == tokPunct && toks[i+1].text == "=" && !punctAt(toks, i+2, "=") ||
			toks[i+1].is("IN")) {
			return true
		}
		if i >= 2 && toks[i-1].kind == tokPunct && toks[i-1].text == "=" &&
			(toks[i-2].kind == tokString || toks[i-2].kind == tokNumber) && !punctAt(toks, i-2, "=") {
			return true
		}
	}
	return false
}

func punctAt(toks []token, i int, p string) bool {
	return i >= 0 && i < len(toks) && toks[i].kind == tokPunct && toks[i].text == p
}

// wrapped 는 가지 전체가 괄호 한 쌍으로 감싸였는지 본다((a) AND (b) 는 아니다).
func wrapped(toks []token) bool {
	if len(toks) < 2 || !punctAt(toks, 0, "(") || !punctAt(toks, len(toks)-1, ")") {
		return false
	}
	depth := 0
	for i, t := range toks {
		if t.kind != tokPunct {
			continue
		}
		switch t.text {
		case "(":
			depth++
		case ")":
			depth--
			if depth == 0 && i != len(toks)-1 {
				return false
			}
		}
	}
	return true
}

func splitTopLevel(toks []token, word string) [][]token {
	var out [][]token
	depth, start := 0, 0
	for i, t := range toks {
		switch {
		case punctAt(toks, i, "(") || punctAt(toks, i, "["):
			depth++
		case punctAt(toks, i, ")") || punctAt(toks, i, "]"):
			depth--
		case depth == 0 && t.is(word):
			out = append(out, toks[start:i])
			start = i + 1
		}
	}
	return append(out, toks[start:])
}

// topLevel 은 from 부터 괄호 밖의 예약어 word 자리를 찾는다. 없으면 -1.
func topLevel(toks []token, from int, word string) int {
	depth := 0
	for i := from; i < len(toks); i++ {
		switch {
		case punctAt(toks, i, "(") || punctAt(toks, i, "["):
			depth++
		case punctAt(toks, i, ")") || punctAt(toks, i, "]"):
			depth--
		case depth == 0 && toks[i].is(word):
			return i
		}
	}
	return -1
}

// isKeyword 는 따옴표 없이 쓰면 이름이 아니라 예약어로 읽히는 낱말이다. 키 이름이 우연히 같을 때
// (예: 키 이름 "IN") 예약어 자리를 키로 잘못 보지 않게 한다.
func isKeyword(w string) bool {
	switch strings.ToUpper(w) {
	case "SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "IN", "BETWEEN", "IS", "ORDER", "BY", "LIMIT":
		return true
	}
	return false
}
