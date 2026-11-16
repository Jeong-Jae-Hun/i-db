// Package flows 는 이벤트 흐름(아웃박스 테이블 → SNS 토픽 → SQS 큐 → DLQ·Lambda) 설정과 아웃박스 수 질의다(명세 R69).
// 설정은 프로젝트의 .dbtui/flows.json 이고 없으면 빈 목록이다. AWS 쪽 풀이는 internal/awsres 가, 화면·CLI 는 main 이 맡는다.
package flows

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// file 은 프로젝트 폴더 기준 설정 파일 자리다. 접속 이름·테이블·열 이름만 들어 레포에 커밋한다.
const file = ".dbtui/flows.json"

// StaleAfter 는 due 열이 이만큼 지난 행을 `오래됨` 으로 센다.
const StaleAfter = "5 minutes"

// Flow 는 흐름 하나다. Outbox 는 없어도 된다(토픽만 보는 흐름). Err 는 검사에서 걸린 까닭이고 있으면 풀지 않는다.
type Flow struct {
	Name   string  `json:"name"`
	Outbox *Outbox `json:"outbox,omitempty"`
	AWS    string  `json:"aws"`
	Topic  string  `json:"topic"`
	Err    string  `json:"-"`
}

// Outbox 는 아웃박스 테이블이다. Table 은 `스키마.테이블` 또는 `테이블`, Type·Retry·Due 는 열 이름이다.
// Where 는 사용자 설정 파일의 SQL 조각이라 편집기 SQL 과 같게 다루되 한 문장·`;` 없음만 받는다.
type Outbox struct {
	Conn  string `json:"conn"`
	Table string `json:"table"`
	Type  string `json:"type"`
	Retry string `json:"retry,omitempty"`
	Due   string `json:"due,omitempty"`
	Where string `json:"where,omitempty"`
}

// Load 는 projectDir 의 설정을 읽는다. 파일이 없으면 빈 목록이다. JSON 이 깨졌으면 오류이고,
// 항목 하나가 틀리면 그 항목 Err 에만 까닭을 적어 나머지는 그대로 쓴다. conns 로 접속 이름·종류를 검사한다.
func Load(projectDir string, conns []db.Conn) ([]Flow, error) {
	data, err := os.ReadFile(filepath.Join(projectDir, file))
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var fs []Flow
	if err := json.Unmarshal(data, &fs); err != nil {
		return nil, fmt.Errorf("%s: %w", file, err)
	}
	seen := map[string]bool{}
	for i := range fs {
		f := &fs[i]
		if err := validate(*f, conns); err != nil {
			f.Err = err.Error()
		} else if seen[f.Name] {
			f.Err = "duplicate name " + f.Name
		}
		seen[f.Name] = true
		if f.Name == "" {
			f.Name = fmt.Sprintf("#%d", i+1)
		}
	}
	return fs, nil
}

func validate(f Flow, conns []db.Conn) error {
	switch {
	case strings.TrimSpace(f.Name) == "":
		return errors.New("name is empty")
	case strings.TrimSpace(f.Topic) == "":
		return errors.New("topic is empty")
	}
	if err := checkConn(conns, f.AWS, db.KindAWS, "aws"); err != nil {
		return err
	}
	if f.Outbox == nil {
		return nil
	}
	o := f.Outbox
	if err := checkConn(conns, o.Conn, db.KindPG, "outbox.conn"); err != nil {
		return err
	}
	for _, c := range []struct{ field, v string }{{"outbox.table", o.Table}, {"outbox.type", o.Type}} {
		if strings.TrimSpace(c.v) == "" {
			return fmt.Errorf("%s is empty", c.field)
		}
	}
	if len(strings.Split(o.Table, ".")) > 2 || strings.HasPrefix(o.Table, ".") || strings.HasSuffix(o.Table, ".") {
		return fmt.Errorf("outbox.table must be table or schema.table: %q", o.Table)
	}
	return checkWhere(o.Where)
}

// checkConn 은 name 이 kind 접속인지다. 없는 이름과 다른 종류를 가른다.
func checkConn(conns []db.Conn, name string, kind db.Kind, field string) error {
	if strings.TrimSpace(name) == "" {
		return fmt.Errorf("%s is empty", field)
	}
	for _, c := range conns {
		if c.Name != name || c.Kind == "" {
			continue
		}
		if c.Kind != kind {
			return fmt.Errorf("%s %s: want kind %s, got %s", field, name, kind, c.Kind)
		}
		return nil
	}
	return fmt.Errorf("%s %s: no such connection", field, name)
}

// checkWhere 는 where 조각이 문장 하나이고 문자열·주석 밖에 `;` 가 없는지다(다른 문장을 이어 붙이지 못하게).
func checkWhere(where string) error {
	if strings.TrimSpace(where) == "" {
		return nil
	}
	if len(db.SplitStatements(where)) != 1 || strings.Contains(db.MaskSQL(where), ";") {
		return errors.New("outbox.where must be a single condition without ';'")
	}
	return nil
}

// ─── 아웃박스 질의 ───

// quoteTable 은 `스키마.테이블` 을 마디마다 따옴표로 감싼다.
func quoteTable(t string) string {
	parts := strings.Split(t, ".")
	for i, p := range parts {
		parts[i] = db.QuoteIdent(p)
	}
	return strings.Join(parts, ".")
}

// filter 는 where 조각을 AND 로 붙인다. 조각 끝의 `--` 주석이 뒤 글을 먹지 않게 줄을 바꿔 괄호로 감싼다.
func filter(o Outbox) string {
	if strings.TrimSpace(o.Where) == "" {
		return ""
	}
	return " where (\n" + o.Where + "\n)"
}

// countExpr 는 retry·due 열이 없을 때 null 을 돌려 `재시도`·`오래됨` 을 안 보이게 한다.
func countExpr(col, cond string) string {
	if col == "" {
		return "null::bigint"
	}
	return fmt.Sprintf("count(*) filter (where %s %s)", db.QuoteIdent(col), cond)
}

// CountSQL 은 대기·재시도·오래됨 수를 한 행으로 묻는다.
func CountSQL(o Outbox) string {
	return fmt.Sprintf("select count(*), %s, %s from %s%s",
		countExpr(o.Retry, "> 0"), countExpr(o.Due, "< now() - interval '"+StaleAfter+"'"), quoteTable(o.Table), filter(o))
}

// TypeSQL 은 상세 보기의 종류별 수 상위 10개다(type·count·retrying·oldest_due).
func TypeSQL(o Outbox) string {
	oldest := "null::timestamptz"
	if o.Due != "" {
		oldest = "min(" + db.QuoteIdent(o.Due) + ")"
	}
	return fmt.Sprintf("select %s as type, count(*) as count, %s as retrying, %s as oldest_due from %s%s group by 1 order by 2 desc, 1 limit 10",
		db.QuoteIdent(o.Type), countExpr(o.Retry, "> 0"), oldest, quoteTable(o.Table), filter(o))
}

// Stat 은 아웃박스 수다. Retry·Stale 이 -1 이면 그 열을 주지 않았다.
type Stat struct {
	Pending, Retry, Stale int64
}

// Count 는 CountSQL 을 READ ONLY 로 돌린다. 제한 시간은 호출부 ctx 가 건다.
func Count(ctx context.Context, pg *db.PGConn, o Outbox) (Stat, error) {
	res, err := pg.RunReadOnly(ctx, CountSQL(o))
	if err != nil {
		return Stat{}, err
	}
	if len(res.Rows) != 1 || len(res.Rows[0]) != 3 {
		return Stat{}, errors.New("unexpected outbox count result")
	}
	var s Stat
	for i, dst := range []*int64{&s.Pending, &s.Retry, &s.Stale} {
		v := res.Rows[0][i]
		if v == "" || v == "NULL" {
			*dst = -1
			continue
		}
		if *dst, err = strconv.ParseInt(v, 10, 64); err != nil {
			return Stat{}, fmt.Errorf("outbox count: %w", err)
		}
	}
	return s, nil
}

// Types 는 TypeSQL 을 READ ONLY 로 돌린 표다.
func Types(ctx context.Context, pg *db.PGConn, o Outbox) (*db.Result, error) {
	return pg.RunReadOnly(ctx, TypeSQL(o))
}

// Metrics 는 `대기 120 · 재시도 3 · 오래됨 1` 이다. 주지 않은 열은 뺀다. warn 은 재시도·오래됨이 0보다 클 때다.
func (s Stat) Metrics() (string, bool) {
	parts := []string{"대기 " + strconv.FormatInt(s.Pending, 10)}
	if s.Retry >= 0 {
		parts = append(parts, "재시도 "+strconv.FormatInt(s.Retry, 10))
	}
	if s.Stale >= 0 {
		parts = append(parts, "오래됨 "+strconv.FormatInt(s.Stale, 10))
	}
	return strings.Join(parts, " · "), s.Retry > 0 || s.Stale > 0
}
