package athena

import (
	"fmt"
	"regexp"
	"slices"
	"strings"

	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── 읽기 판정 (R68.2) ───

const (
	fullScan    = "전체 스캔"
	noPartition = "파티션 조건 없음"
)

// readStarts 는 보낼 수 있는 첫 낱말이다. CTAS(CREATE TABLE AS)·INSERT·UNLOAD·MSCK 같은 쓰기는 첫 낱말에서 걸린다.
var readStarts = []string{"select", "with", "show", "describe", "explain"}

// writeWords 는 SELECT·WITH·EXPLAIN 문장 어디에(하위 질의 포함) 있어도 거절하는 낱말이다. `EXPLAIN ANALYZE INSERT …` 는
// 실제로 쓰므로 막는다. 따옴표 없는 같은 이름의 열도 걸리지만 안전 쪽이다("update" 처럼 따옴표로 감싸면 된다).
// SHOW·DESCRIBE 는 쓰는 꼴이 없어 보지 않는다(`SHOW CREATE TABLE` 을 받으려고).
var writeWords = []string{"insert", "update", "delete", "merge", "create", "drop", "alter", "truncate", "msck",
	"unload", "vacuum", "optimize", "grant", "revoke", "call", "prepare", "execute", "deallocate"}

// readOnly 는 text 가 읽기 문장 하나인지 보고 보낼 글과 낱말을 돌려준다. 아니면 source.ErrWrite 다.
func readOnly(text string) (string, []string, error) {
	stmt, words, err := source.SQLWords(text)
	if err != nil {
		return "", nil, err
	}
	if len(words) == 0 || !slices.Contains(readStarts, words[0]) {
		first := ""
		if len(words) > 0 {
			first = words[0]
		}
		return "", nil, fmt.Errorf("%w: %s (only SELECT, WITH, SHOW, DESCRIBE, EXPLAIN)", source.ErrWrite, first)
	}
	if words[0] != "show" && words[0] != "describe" {
		for _, w := range words {
			if slices.Contains(writeWords, w) {
				return "", nil, fmt.Errorf("%w: %s", source.ErrWrite, w)
			}
		}
	}
	return stmt, words, nil
}

// expensive 는 확인받을 까닭이다(R60.2). 표를 읽는 문장(SELECT·WITH·EXPLAIN ANALYZE)에 LIMIT 도 WHERE 도 없으면
// 전체 스캔으로 본다. 문장 어디든(하위 질의 포함) LIMIT·WHERE 낱말이 있으면 넘긴다: 파티션 조건인지까지 보지 않는
// 가벼운 안전장치이고, 나머지는 작업 그룹의 스캔 상한(BytesScannedCutoffPerQuery)이 막는다.
func expensive(words []string) string {
	scans := words[0] == "select" || words[0] == "with" || words[0] == "explain" && len(words) > 1 && words[1] == "analyze"
	if scans && !slices.Contains(words, "limit") && !slices.Contains(words, "where") {
		return fullScan
	}
	return ""
}

var databaseRe = regexp.MustCompile(`(?i)^--\s*database:\s*(\S+)\s*$`)

// databaseDirective 는 첫 글줄의 `-- database: <이름>` 을 읽어 질의 맥락 데이터베이스로 쓰고 그 줄을 뗀 글을 돌려준다.
// 저장소 탭에는 고른 데이터베이스가 없어서다. 없으면 "" 이고 이름을 "db"."table" 처럼 다 적는다(R68.2).
// 줄을 떼는 까닭: Athena 는 앞머리 주석이 있으면 SHOW·DESCRIBE 같은 DDL 을 읽지 못하는 일이 있다.
func databaseDirective(text string) (string, string) {
	trimmed := strings.TrimLeft(text, " \t\r\n")
	line, rest, _ := strings.Cut(trimmed, "\n")
	m := databaseRe.FindStringSubmatch(strings.TrimRight(line, "\r"))
	if m == nil {
		return "", text
	}
	return strings.Trim(m[1], "\"`"), rest
}
