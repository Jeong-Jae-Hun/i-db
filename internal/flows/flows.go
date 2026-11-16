// Package flows 는 이벤트 흐름(아웃박스 테이블 → SNS 토픽 → SQS 큐 → DLQ·Lambda, 또는 아웃박스 → 종류별 소비자) 설정과 아웃박스 수 질의다(명세 R69).
// 설정은 프로젝트의 .dbtui/flows.json 이고 없으면 빈 목록이다. AWS 쪽 풀이는 internal/awsres 가, 화면·CLI 는 main 이 맡는다.
package flows

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// file 은 프로젝트 폴더 기준 설정 파일 자리다. 접속 이름·테이블·열 이름만 들어 레포에 커밋한다.
const file = ".dbtui/flows.json"

const (
	// StaleAfter 는 due 열이 이만큼 지난 행을 `오래됨` 으로 센다.
	StaleAfter = "5 minutes"
	// DefaultLimit 은 limit 을 주지 않은 아웃박스의 재시도 한도다. retry 가 한도에 닿은 행은 다시 집어 가지 않는다.
	DefaultLimit = 5
	// RetryRows 는 종류 상세(재시도 중·한도 소진 행)의 최대 행 수다.
	RetryRows = 50
)

// Flow 는 흐름 하나다. Outbox·Topic 가운데 하나는 있어야 하고, AWS 는 Topic 이 있을 때만 쓴다.
// Err 는 검사에서 걸린 까닭이고 있으면 풀지 않는다.
type Flow struct {
	Name   string  `json:"name"`
	Outbox *Outbox `json:"outbox,omitempty"`
	AWS    string  `json:"aws,omitempty"`
	Topic  string  `json:"topic,omitempty"`
	Err    string  `json:"-"`
}

// Outbox 는 아웃박스 테이블이다. Table 은 `스키마.테이블` 또는 `테이블`, Type·Retry·Due 는 열 이름이다.
// Where 는 사용자 설정 파일의 SQL 조각이라 편집기 SQL 과 같게 다루되 한 문장·`;` 없음만 받는다.
// Limit 은 기본 재시도 한도(없으면 DefaultLimit), Types 는 종류별 한도·소비자다. 둘 다 Retry 가 있어야 한다.
// Limit 이 포인터인 까닭은 주지 않음과 잘못 준 0 을 가르기 위해서다.
type Outbox struct {
	Conn  string              `json:"conn"`
	Table string              `json:"table"`
	Type  string              `json:"type"`
	Retry string              `json:"retry,omitempty"`
	Due   string              `json:"due,omitempty"`
	Where string              `json:"where,omitempty"`
	Limit *int                `json:"limit,omitempty"`
	Types map[string]TypeConf `json:"types,omitempty"`
}

// TypeConf 는 이벤트 종류 하나의 설정이다. Limit 이 없으면 Outbox 한도, Consumer 는 그 종류를 가져가는 잡 이름(보이기만 한다)이다.
type TypeConf struct {
	Limit    *int   `json:"limit,omitempty"`
	Consumer string `json:"consumer,omitempty"`
}

// limit 은 기본 재시도 한도다.
func (o Outbox) limit() int {
	if o.Limit != nil {
		return *o.Limit
	}
	return DefaultLimit
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
	case f.Outbox == nil && strings.TrimSpace(f.Topic) == "":
		return errors.New("outbox or topic is required")
	case strings.TrimSpace(f.Topic) == "" && f.AWS != "":
		return errors.New("aws requires topic")
	}
	if strings.TrimSpace(f.Topic) != "" {
		if err := checkConn(conns, f.AWS, db.KindAWS, "aws"); err != nil {
			return err
		}
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
	if err := checkLimits(*o); err != nil {
		return err
	}
	return checkWhere(o.Where)
}

// checkLimits 는 한도가 1 이상이고 종류 이름이 비지 않았는지, 한도·종류 설정에 retry 열이 있는지다.
func checkLimits(o Outbox) error {
	switch {
	case o.Retry == "" && o.Limit != nil:
		return errors.New("outbox.limit requires outbox.retry")
	case o.Retry == "" && len(o.Types) > 0:
		return errors.New("outbox.types requires outbox.retry")
	case o.Limit != nil && *o.Limit < 1:
		return errors.New("outbox.limit must be >= 1")
	}
	for _, k := range slices.Sorted(maps.Keys(o.Types)) {
		if strings.TrimSpace(k) == "" {
			return errors.New("outbox.types has an empty type")
		}
		if l := o.Types[k].Limit; l != nil && *l < 1 {
			return fmt.Errorf("outbox.types.%s.limit must be >= 1", k)
		}
	}
	return nil
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

// filter 는 조건을 AND 로 잇고 where 조각을 붙인다. 조각 끝의 `--` 주석이 뒤 글을 먹지 않게 줄을 바꿔 괄호로 감싼다.
func filter(o Outbox, conds ...string) string {
	if strings.TrimSpace(o.Where) != "" {
		conds = append(conds, "(\n"+o.Where+"\n)")
	}
	if len(conds) == 0 {
		return ""
	}
	return " where " + strings.Join(conds, " and ")
}

// typeText 는 종류 열을 글로 바꾼다. enum 열이면 설정에만 있는 종류 이름 리터럴이 형 오류를 내지 않게 한다.
func typeText(o Outbox) string { return db.QuoteIdent(o.Type) + "::text" }

// retryExpr 는 NULL 재시도를 0 으로 본다(아직 한 번도 안 돈 행).
func retryExpr(o Outbox) string { return "coalesce(" + db.QuoteIdent(o.Retry) + ", 0)" }

// limitExpr 는 행마다의 재시도 한도다. 종류별 한도가 있으면 `case <type> when 'X' then 20 … else <limit> end` 이고
// 종류 이름은 db.QuoteLiteral 로 감싼다. 같은 설정이면 같은 SQL 이 나오게 종류 이름 순이다.
func limitExpr(o Outbox) string {
	def := strconv.Itoa(o.limit())
	var b strings.Builder
	for _, k := range slices.Sorted(maps.Keys(o.Types)) {
		if l := o.Types[k].Limit; l != nil {
			fmt.Fprintf(&b, " when %s then %d", db.QuoteLiteral(k), *l)
		}
	}
	if b.Len() == 0 {
		return def
	}
	return "case " + typeText(o) + b.String() + " else " + def + " end"
}

// CountSQL 은 종류별 수를 한 문장으로 묻는다(type·rows·waiting·retrying·exhausted·stale·oldest_due).
// retry 가 있으면 대기는 retry < 한도, 재시도는 0 < retry < 한도, 한도 소진은 retry >= 한도, 오래됨은 due 가 지났고 retry < 한도다
// (한도 소진 행은 다시 집어 가지 않으니 늦은 게 아니라 버려진 것이다). retry 가 없으면 대기는 모든 행, 재시도·한도 소진은 null 이다.
// 행 수 제한을 두지 않는 까닭은 합을 이 결과로 내기 때문이다. 종류는 수십 개라 결과 행 상한(db.MaxRows)에 닿지 않는다.
func CountSQL(o Outbox) string {
	waiting, retrying, exhausted, stale, oldest := "count(*)", "null::bigint", "null::bigint", "null::bigint", "null::timestamptz"
	live := ""
	if o.Retry != "" {
		r, l := retryExpr(o), limitExpr(o)
		live = r + " < " + l
		waiting = "count(*) filter (where " + live + ")"
		retrying = "count(*) filter (where " + r + " > 0 and " + live + ")"
		exhausted = "count(*) filter (where " + r + " >= " + l + ")"
	}
	if o.Due != "" {
		due := db.QuoteIdent(o.Due)
		cond := due + " < now() - interval '" + StaleAfter + "'"
		oldest = "min(" + due + ")"
		if live != "" {
			cond += " and " + live
			oldest += " filter (where " + live + ")"
		}
		stale = "count(*) filter (where " + cond + ")"
	}
	return fmt.Sprintf("select %s as type, count(*) as rows, %s as waiting, %s as retrying, %s as exhausted, %s as stale, %s as oldest_due from %s%s group by 1 order by 5 desc nulls last, 3 desc, 1",
		typeText(o), waiting, retrying, exhausted, stale, oldest, quoteTable(o.Table), filter(o))
}

// RetrySQL 은 종류 하나의 재시도 중·한도 소진 행(retry > 0) 최근 RetryRows 개다. 열은 테이블 그대로다.
// retry 가 없으면 그 종류의 행을 due 순으로 보인다.
func RetrySQL(o Outbox, typ string) string {
	conds := []string{typeText(o) + " = " + db.QuoteLiteral(typ)}
	var order []string
	if o.Retry != "" {
		conds = append(conds, retryExpr(o)+" > 0")
		order = append(order, retryExpr(o)+" desc")
	}
	if o.Due != "" {
		order = append(order, db.QuoteIdent(o.Due))
	}
	q := "select * from " + quoteTable(o.Table) + filter(o, conds...)
	if len(order) > 0 {
		q += " order by " + strings.Join(order, ", ")
	}
	return q + " limit " + strconv.Itoa(RetryRows)
}

// TypeStat 은 종류 하나의 수다. Retrying·Exhausted·Stale 이 -1 이면 그 열을 주지 않았다.
type TypeStat struct {
	Name                                      string
	Rows, Waiting, Retrying, Exhausted, Stale int64
	OldestDue                                 string
}

// Stat 은 아웃박스 수다(종류별 합). Retry·Exhausted·Stale 이 -1 이면 그 열을 주지 않았다.
type Stat struct {
	Pending, Retry, Stale, Exhausted int64
	Types                            []TypeStat
}

// Count 는 CountSQL 을 READ ONLY 로 돌려 종류별 수와 그 합을 낸다. 제한 시간은 호출부 ctx 가 건다.
func Count(ctx context.Context, pg *db.PGConn, o Outbox) (Stat, error) {
	res, err := pg.RunReadOnly(ctx, CountSQL(o))
	if err != nil {
		return Stat{}, err
	}
	s := Stat{Retry: -1, Stale: -1, Exhausted: -1}
	for _, row := range res.Rows {
		if len(row) != 7 {
			return Stat{}, errors.New("unexpected outbox count result")
		}
		t := TypeStat{Name: row[0], OldestDue: row[6]}
		for i, dst := range []*int64{&t.Rows, &t.Waiting, &t.Retrying, &t.Exhausted, &t.Stale} {
			v := row[i+1]
			if v == "" || v == "NULL" {
				*dst = -1
				continue
			}
			if *dst, err = strconv.ParseInt(v, 10, 64); err != nil {
				return Stat{}, fmt.Errorf("outbox count: %w", err)
			}
		}
		s.Pending += t.Waiting
		for _, p := range []struct {
			sum *int64
			v   int64
		}{{&s.Retry, t.Retrying}, {&s.Exhausted, t.Exhausted}, {&s.Stale, t.Stale}} {
			if p.v >= 0 {
				*p.sum = max(0, *p.sum) + p.v
			}
		}
		s.Types = append(s.Types, t)
	}
	// 행이 없으면 열이 있어도 수는 0 이다
	if len(res.Rows) == 0 {
		if o.Retry != "" {
			s.Retry, s.Exhausted = 0, 0
		}
		if o.Due != "" {
			s.Stale = 0
		}
	}
	return s, nil
}

// Metrics 는 `대기 120 · 재시도 3 · 한도 소진 2 · 오래됨 1` 이다. 주지 않은 열은 뺀다. warn 은 재시도·한도 소진·오래됨이 0보다 클 때다.
func (s Stat) Metrics() (string, bool) {
	parts := []string{"대기 " + strconv.FormatInt(s.Pending, 10)}
	if s.Retry >= 0 {
		parts = append(parts, "재시도 "+strconv.FormatInt(s.Retry, 10))
	}
	if s.Exhausted >= 0 {
		parts = append(parts, "한도 소진 "+strconv.FormatInt(s.Exhausted, 10))
	}
	if s.Stale >= 0 {
		parts = append(parts, "오래됨 "+strconv.FormatInt(s.Stale, 10))
	}
	return strings.Join(parts, " · "), s.Retry > 0 || s.Stale > 0 || s.Exhausted > 0
}

// Metrics 는 종류 줄의 `대기 3 · 재시도 1 · 한도 소진 2` 다. 오래됨은 있을 때만 붙인다. warn 은 한도 소진·오래됨이 0보다 클 때다.
func (t TypeStat) Metrics() (string, bool) {
	parts := []string{"대기 " + strconv.FormatInt(t.Waiting, 10)}
	if t.Retrying >= 0 {
		parts = append(parts, "재시도 "+strconv.FormatInt(t.Retrying, 10))
	}
	if t.Exhausted >= 0 {
		parts = append(parts, "한도 소진 "+strconv.FormatInt(t.Exhausted, 10))
	}
	if t.Stale > 0 {
		parts = append(parts, "오래됨 "+strconv.FormatInt(t.Stale, 10))
	}
	return strings.Join(parts, " · "), t.Exhausted > 0 || t.Stale > 0
}
