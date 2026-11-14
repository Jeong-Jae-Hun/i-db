package db

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"strings"

	"github.com/jackc/pgx/v5"
)

var explainFormats = map[string]bool{"TEXT": true, "JSON": true, "YAML": true, "XML": true}

// ExplainOpt 의 Format 이 비었거나 TEXT·JSON·YAML·XML 이 아니면 TEXT 로 본다.
type ExplainOpt struct {
	Analyze, Buffers bool
	Format           string
}

// ExplainSQL 은 문장 하나를 EXPLAIN (옵션) 으로 감싼다. 끝 ';' 는 뗀다.
// 이미 EXPLAIN 으로 시작하면 사용자가 쓴 옵션을 그대로 두려고 감싸지 않는다.
// ANALYZE 는 문장을 실제로 실행하므로 확인은 호출하는 쪽이 IsReadOnlySQL 로 한다.
func ExplainSQL(stmt string, opt ExplainOpt) string {
	stmt = trimStatement(stmt)
	if stmt == "" || FirstSQLWord(stmt) == "explain" {
		return stmt
	}
	var opts []string
	if opt.Analyze {
		opts = append(opts, "ANALYZE")
	}
	if opt.Buffers {
		opts = append(opts, "BUFFERS")
	}
	// Format 을 그대로 이어 붙이면 SQL 조각이 끼어들 수 있어 허용 목록만 받는다
	format := strings.ToUpper(strings.TrimSpace(opt.Format))
	if !explainFormats[format] {
		format = "TEXT"
	}
	opts = append(opts, "FORMAT "+format)
	// 문장이 줄 주석으로 끝나도 EXPLAIN 이 앞에 붙으므로 괜찮다
	return "EXPLAIN (" + strings.Join(opts, ", ") + ") " + stmt
}

// planNode 는 EXPLAIN (FORMAT JSON) 의 노드에서 영향 행 수를 찾는 데 쓰는 필드만 읽는다.
type planNode struct {
	NodeType string     `json:"Node Type"`
	Relation string     `json:"Parent Relationship"`
	Rows     float64    `json:"Plan Rows"`
	Plans    []planNode `json:"Plans"`
}

// EstimateRows 는 UPDATE·DELETE 문장 하나가 바꿀 행 수의 플래너 추정치다(명세 R39). EXPLAIN 만 하고 문장은 실행하지 않는다.
// 편집기 세션이 아니라 풀 연결의 읽기 전용 트랜잭션에서 돌린다. 확인 창을 띄운 채 세션을 잡지 않고, SET LOCAL 로 맞춘
// search_path 가 풀 연결에 남지 않게 하려서다. ANALYZE 없는 EXPLAIN 은 READ ONLY 트랜잭션에서도 쓰기 문장을 받는다.
// 시간 제한은 ctx 로 준다.
func (p *PGConn) EstimateRows(ctx context.Context, sql string) (int64, error) {
	stmts := SplitStatements(sql)
	if len(stmts) != 1 {
		return 0, errors.New("estimate needs exactly one statement")
	}
	stmt := trimStatement(stmts[0])
	if w := FirstSQLWord(stmt); w != "update" && w != "delete" {
		return 0, errors.New("estimate supports only UPDATE or DELETE")
	}
	// 문장 뒤에 붙이므로 EXPLAIN 옵션이 될 수는 없지만, 실행으로 이어질 수 있는 단어는 아예 받지 않는다
	if analyzeRe.MatchString(MaskSQL(stmt)) {
		return 0, errors.New("statement must not contain ANALYZE")
	}
	tx, err := p.pool.BeginTx(ctx, pgx.TxOptions{AccessMode: pgx.ReadOnly})
	if err != nil {
		return 0, err
	}
	// 롤백이 실패하면 pgxpool 이 트랜잭션 중인 연결을 풀에 돌려주지 않고 닫는다
	defer func() { _ = tx.Rollback(context.Background()) }()
	if sp := p.SearchPath(); sp != "" {
		if _, err := tx.Exec(ctx, "SET LOCAL "+strings.TrimPrefix(searchPathSQL(sp), "SET ")); err != nil {
			return 0, fmt.Errorf("set search_path: %w", err)
		}
	}
	var raw []byte
	// 사용자 문장이라 풀 연결의 문장 캐시에 남기지 않는다. extended protocol 이라 여러 문장은 서버가 거부한다
	if err := tx.QueryRow(ctx, ExplainSQL(stmt, ExplainOpt{Format: "JSON"}), pgx.QueryExecModeDescribeExec).Scan(&raw); err != nil {
		return 0, err
	}
	return modifyTableRows(raw)
}

// modifyTableRows 는 맨 위 ModifyTable 노드의 Outer 자식(바꿀 행을 만드는 노드)의 Plan Rows 다.
// PostgreSQL 13 이하는 상속·파티션 테이블을 바꿀 때 자식마다 Member 자식을 두므로 Outer 가 없으면 Member 의 합이다.
// ModifyTable 자신의 Plan Rows 는 RETURNING 이 없으면 0 이라 쓰지 않는다. InitPlan·SubPlan 자식은 건너뛴다.
func modifyTableRows(raw []byte) (int64, error) {
	var plans []struct{ Plan planNode }
	if err := json.Unmarshal(raw, &plans); err != nil {
		return 0, fmt.Errorf("parse EXPLAIN JSON: %w", err)
	}
	if len(plans) != 1 || plans[0].Plan.NodeType != "ModifyTable" {
		return 0, errors.New("EXPLAIN top node is not ModifyTable")
	}
	members, sum := 0, 0.0
	for _, c := range plans[0].Plan.Plans {
		switch c.Relation {
		case "Outer":
			return int64(math.Round(c.Rows)), nil
		case "Member":
			members++
			sum += c.Rows
		}
	}
	if members > 0 {
		return int64(math.Round(sum)), nil
	}
	return 0, errors.New("EXPLAIN ModifyTable has no outer plan")
}
