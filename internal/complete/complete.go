// Package complete 는 SQL·Redis 편집기 자동완성 후보를 만든다.
// db 패키지에 기대지 않도록 토크나이저를 따로 둔다. db 가 이 패키지의 Catalog 를 채운다.
package complete

import "strings"

// Item.Kind 값
const (
	KindKeyword  = "keyword"
	KindTable    = "table"
	KindView     = "view"
	KindColumn   = "column"
	KindSchema   = "schema"
	KindFunction = "function"
	KindCommand  = "command"
	KindKey      = "key"
)

const maxItems = 50

// Catalog 는 자동완성에 쓰는 PostgreSQL 이름 목록이다. 스키마 → 테이블·뷰 → 열.
type Catalog struct {
	Schemas   []Schema
	Functions []string
}

type Schema struct {
	Name   string
	Tables []Table
}

type Table struct {
	Name    string
	View    bool
	Columns []Column
}

type Column struct {
	Name, Type string
}

// Item 은 후보 하나다. 편집기는 커서 바로 앞 PrefixLen 바이트를 Insert 로 바꿔 넣는다.
// PrefixLen 은 엔진이 접두어로 본 원문 길이라 여는 따옴표, 따옴표 안 공백, Redis 키의 ':' 까지 센다.
type Item struct {
	Label     string
	Kind      string
	Detail    string
	Insert    string
	PrefixLen int
}

// Query 의 Offset 은 Text 안 커서 위치(바이트)다.
type Query struct {
	Text   string
	Offset int
}

// typed 는 커서 앞에서 치고 있는 단어다. name 은 따옴표를 벗긴 이름이고 size 는 원문 바이트 수다.
type typed struct {
	name string
	size int
}

// rank 는 접두어로 거르고 접두 일치를 부분 일치보다 앞에 둔다. 같은 등급 안에서는 입력 순서를 지킨다.
// Label 이 대소문자까지 같은 후보는 앞의 것만 남긴다. 열 id 와 "ID", Redis 키 Foo 와 foo 는 서로 다른 대상이다.
// 스키마로 한정한 테이블("스키마.테이블")만 테이블 이름의 접두 일치도 본다. 점이 든 Redis 키는 키 전체로 맞춘다.
// 돌려주는 후보의 PrefixLen 은 w.size 다.
func rank(items []Item, w typed) []Item {
	p := strings.ToLower(w.name)
	seen := map[string]bool{}
	var head, tail []Item
	for _, it := range items {
		if seen[it.Label] {
			continue
		}
		key, short := strings.ToLower(it.Label), ""
		if (it.Kind == KindTable || it.Kind == KindView) && it.Detail != "" {
			short = strings.TrimPrefix(key, strings.ToLower(it.Detail)+".")
		}
		it.PrefixLen = w.size
		switch {
		case strings.HasPrefix(key, p) || short != "" && strings.HasPrefix(short, p):
			head = append(head, it)
		case strings.Contains(key, p):
			tail = append(tail, it)
		default:
			continue
		}
		seen[it.Label] = true
		if len(head) >= maxItems {
			break
		}
	}
	out := append(head, tail...)
	return out[:min(len(out), maxItems)]
}

// caseLike 는 사용자가 소문자로 치고 있으면 소문자로, 아니면 대문자로 넣는다.
func caseLike(word, prefix string) string {
	if prefix != "" && prefix == strings.ToLower(prefix) && prefix != strings.ToUpper(prefix) {
		return strings.ToLower(word)
	}
	return strings.ToUpper(word)
}

func clampOffset(q Query) int {
	return max(0, min(q.Offset, len(q.Text)))
}
