package db

import (
	"context"
	"errors"
	"fmt"
	"maps"
	"slices"
	"strings"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"
)

// 편집 묶음은 세션 트랜잭션 밖이면 BEGIN…COMMIT 으로, 안이면 SAVEPOINT 로 감싼다.
// 실패해도 사용자가 연 바깥 트랜잭션은 닫지 않고 이 묶음만 되돌린다.
const (
	editSavepoint         = "SAVEPOINT dbtui_edit"
	editReleaseSavepoint  = "RELEASE SAVEPOINT dbtui_edit"
	editRollbackSavepoint = "ROLLBACK TO SAVEPOINT dbtui_edit; RELEASE SAVEPOINT dbtui_edit"
)

var changeVerbs = map[ChangeOp]string{ChangeUpdate: "UPDATE", ChangeInsert: "INSERT", ChangeDelete: "DELETE"}

// sqlBuilder 는 같은 문장을 바인딩($n)으로도, 미리 보기용 리터럴로도 만든다. noReturning 이면 RETURNING * 를 붙이지 않는다.
type sqlBuilder struct {
	strings.Builder
	args        []any
	inline      bool
	noReturning bool
}

// ApplyChanges 는 편집 묶음을 편집기 세션에서 한 번에 반영한다. UPDATE·DELETE 가 정확히 한 행을 바꾸지 않으면 전부 되돌린다.
// INSERT 가 돌려준 행(기본값이 채워진 행)은 결과 표로 싣고, 바꿈마다 RETURNING * 로 받은 행은 Applied 에 싣는다(R50 되돌리기).
// RETURNING * 가 권한(열 단위 GRANT·RLS, insufficient_privilege)으로 거절되면 묶음째 되돌린 뒤 RETURNING 없이 한 번 더 내고
// Applied.NoReturning 을 켠다. 그 묶음은 되살릴 행이 없어 되돌릴 수 없다. 바꾸는 권한 자체가 없으면 두 번째도 같은 오류다.
func (p *PGConn) ApplyChanges(ctx context.Context, cs ChangeSet) (*Result, error) {
	res, err := p.applyChanges(ctx, cs, true)
	var pgErr *pgconn.PgError
	if !errors.As(err, &pgErr) || pgErr.Code != "42501" { // insufficient_privilege
		return res, err
	}
	return p.applyChanges(ctx, cs, false)
}

func (p *PGConn) applyChanges(ctx context.Context, cs ChangeSet, returning bool) (res *Result, err error) {
	if len(cs.Changes) == 0 {
		return nil, errors.New("no changes to apply")
	}
	start := time.Now()
	c, err := p.openSession(ctx)
	if err != nil {
		return nil, err
	}
	defer func() { res, err = p.sess.release(res, err) }()
	if c.Conn().PgConn().TxStatus() == 'E' {
		return nil, errors.New("current transaction is aborted; roll back before applying changes")
	}
	begin, commit, rollback := "BEGIN", "COMMIT", "ROLLBACK"
	if txOpen(c) {
		begin, commit, rollback = editSavepoint, editReleaseSavepoint, editRollbackSavepoint
	}
	if _, err = c.Exec(ctx, begin); err != nil {
		return nil, err
	}
	defer func() {
		if err == nil || c.Conn().IsClosed() {
			return
		}
		// 취소된 ctx 로는 되돌리기도 못 보내므로 새 마감을 쓴다
		rctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if _, rerr := c.Exec(rctx, rollback); rerr != nil {
			err = errors.Join(err, fmt.Errorf("rollback: %w", rerr))
		}
	}()

	res = &Result{Applied: &Applied{Rows: make([]map[string]*string, len(cs.Changes)), NoReturning: !returning}}
	if slices.ContainsFunc(cs.Changes, func(ch Change) bool { return ch.Op == ChangeDelete }) {
		// 지운 행을 되살릴 때 생성 열은 값을 넣을 수 없어 빼야 한다. 문장 캐시는 lookupRel 과 같은 이유로 쓰지 않는다
		err = c.QueryRow(ctx, `select coalesce(array_agg(attname::text order by attnum), '{}') from pg_attribute
			where attrelid = $1::regclass and attnum > 0 and not attisdropped and attgenerated <> ''`,
			pgx.QueryExecModeDescribeExec, cs.Table.Quoted()).Scan(&res.Applied.Generated)
		if err != nil {
			return nil, err
		}
	}
	counts := map[ChangeOp]int{}
	// 바꿈마다 영향 행 수를 확인하고 첫 실패에서 멈춰야 해서 하나씩 보낸다
	for i, ch := range cs.Changes {
		b := sqlBuilder{noReturning: !returning}
		if err = cs.build(i, &b); err != nil {
			return nil, err
		}
		args := append([]any{pgx.QueryExecModeDescribeExec, pgx.QueryResultFormats{pgx.TextFormatCode}}, b.args...)
		rows, err := c.Query(ctx, b.String(), args...)
		if err != nil {
			return nil, err
		}
		fields := rows.FieldDescriptions()
		// RETURNING * 의 행을 자르지 않고 NULL 을 가려 남긴다. 되돌리기가 이 값으로 행을 찾고 되살린다
		got := collectRows(rows, MaxRows, func(vals [][]byte) {
			post := make(map[string]*string, len(vals))
			for j, v := range vals {
				if v != nil {
					s := string(v)
					post[fields[j].Name] = &s
				} else {
					post[fields[j].Name] = nil
				}
			}
			res.Applied.Rows[i] = post
		})
		if err := rows.Err(); err != nil {
			return nil, err
		}
		n := rows.CommandTag().RowsAffected()
		// 지운 행을 되살리는 INSERT(되돌리기)는 BEFORE 트리거가 NULL 을 돌려주면 0행이 되고도 성공한다. 되살렸다고 알리지 않게 막는다
		if ch.Op == ChangeInsert && cs.Override && (n != 1 || returning && res.Applied.Rows[i] == nil) {
			return nil, fmt.Errorf("INSERT restoring a deleted row affected %d rows, expected 1 (a trigger may have skipped it); no changes were applied", n)
		}
		if ch.Op != ChangeInsert && n != 1 {
			// 오류에 실을 키는 PK 순서를 지켜야 해서 맵을 돌지 않고 cs.PK 를 따라 만든다
			key := make([]string, len(cs.PK))
			for j, k := range cs.PK {
				key[j] = k + "=" + ch.Key[k]
			}
			if n == 0 {
				return nil, fmt.Errorf("%s failed: row (%s) changed since it was read or was deleted; no changes were applied", changeVerbs[ch.Op], strings.Join(key, ", "))
			}
			return nil, fmt.Errorf("%s of row (%s) affected %d rows, expected 1; no changes were applied", changeVerbs[ch.Op], strings.Join(key, ", "), n)
		}
		if ch.Op == ChangeInsert {
			res.Cols, res.Types = got.Cols, got.Types
			res.Rows = append(res.Rows, got.Rows...)
		}
		counts[ch.Op]++
	}
	if _, err = c.Exec(ctx, commit); err != nil {
		if commit == "COMMIT" {
			// SAVEPOINT 해제가 끊기면 바깥 트랜잭션째 사라져 결과가 분명하지만, COMMIT 은 서버가 이미 반영했을 수 있다
			err = outcomeUnknown("commit", err)
		}
		return nil, err
	}
	var parts []string
	// 요약은 삭제·추가 순서가 아니라 고정된 UPDATE·INSERT·DELETE 순서로 보여 주려고 op 목록을 돈다
	for _, op := range []ChangeOp{ChangeUpdate, ChangeInsert, ChangeDelete} {
		if counts[op] > 0 {
			parts = append(parts, fmt.Sprintf("%s %d", changeVerbs[op], counts[op]))
		}
	}
	res.Text = strings.Join(parts, " · ")
	res.Elapsed = time.Since(start)
	return res, nil
}

// ChangesSQL 은 확인 창에 보여 줄 문장이다. 값을 리터럴로 넣어 읽기 쉽게 만들 뿐 실행하지 않는다.
func ChangesSQL(cs ChangeSet) []string {
	out := make([]string, len(cs.Changes))
	// 실패한 바꿈도 그 자리에 오류 주석으로 남겨 실행 순서와 줄을 맞춘다
	for i := range cs.Changes {
		b := sqlBuilder{inline: true}
		if err := cs.build(i, &b); err != nil {
			out[i] = "-- " + err.Error()
			continue
		}
		out[i] = b.String() + ";"
	}
	return out
}

// build 는 i 번째 바꿈의 문장을 b 에 쓴다. 열 순서는 이름순이라 미리 보기와 실행이 같은 문장이 된다.
func (cs ChangeSet) build(i int, b *sqlBuilder) error {
	ch := cs.Changes[i]
	cols := slices.Sorted(maps.Keys(ch.Values))
	switch ch.Op {
	case ChangeInsert:
		if len(cols) == 0 {
			b.WriteString("INSERT INTO " + cs.Table.Quoted() + " DEFAULT VALUES")
			b.returning()
			return nil
		}
		quoted := make([]string, len(cols))
		// 열 이름과 값 자리를 같은 이름순으로 써야 짝이 맞는다
		for j, col := range cols {
			quoted[j] = QuoteIdent(col)
		}
		b.WriteString("INSERT INTO " + cs.Table.Quoted() + " (" + strings.Join(quoted, ", ") + ")")
		if cs.Override {
			b.WriteString(" OVERRIDING SYSTEM VALUE")
		}
		b.WriteString(" VALUES (")
		// 값 자리 번호($n)가 쓰는 순서대로 매겨져서 문자열을 이어 쓰며 돈다
		for j, col := range cols {
			if j > 0 {
				b.WriteString(", ")
			}
			b.value(ch.Values[col])
		}
		b.WriteString(")")
		b.returning()
		return nil
	case ChangeUpdate:
		if len(cols) == 0 {
			return errors.New("update has no changed columns")
		}
		b.WriteString("UPDATE " + cs.Table.Quoted() + " SET ")
		// 값 자리 번호($n)가 쓰는 순서대로 매겨져서 문자열을 이어 쓰며 돈다
		for j, col := range cols {
			if j > 0 {
				b.WriteString(", ")
			}
			b.WriteString(QuoteIdent(col) + " = ")
			b.value(ch.Values[col])
		}
	case ChangeDelete:
		b.WriteString("DELETE FROM " + cs.Table.Quoted())
	default:
		return fmt.Errorf("unknown change op %d", ch.Op)
	}
	if len(cs.PK) == 0 {
		return errors.New("change set has no primary key")
	}
	// WHERE 와 AND 를 가르고 PK 순서를 지키려고 인덱스로 돈다
	for j, k := range cs.PK {
		v, ok := ch.Key[k]
		if !ok {
			return fmt.Errorf("row key is missing primary key column %q", k)
		}
		if j == 0 {
			b.WriteString(" WHERE ")
		} else {
			b.WriteString(" AND ")
		}
		b.WriteString(QuoteIdent(k) + " = ")
		b.value(&v)
	}
	// 옛값은 읽을 때 받은 텍스트 표기와 견준다. 열 타입으로 = 를 쓰면 json·point 처럼 같음 연산자가 없는 타입에서 실패하고,
	// ::text 는 bool('true')·inet('/32')처럼 출력 함수와 다른 캐스트가 있어 어긋난다. format('%s') 는 출력 함수를 쓴다.
	// format 은 NULL 을 '' 로 바꾸므로 NULL 여부를 따로 본다. 이름순으로 돌아 미리 보기와 실행이 같은 문장이 된다
	for _, col := range slices.Sorted(maps.Keys(ch.Old)) {
		v := ch.Old[col]
		if v == nil {
			b.WriteString(" AND " + QuoteIdent(col) + " IS NULL")
			continue
		}
		b.WriteString(" AND " + QuoteIdent(col) + " IS NOT NULL AND format('%s', " + QuoteIdent(col) + ") = ")
		b.value(v)
	}
	b.returning()
	return nil
}

func (b *sqlBuilder) returning() {
	if !b.noReturning {
		b.WriteString(" RETURNING *")
	}
}

// outcomeUnknown 은 결과를 확정하는 문장(COMMIT, 자동 커밋 COPY)의 오류가 서버 판정이 아니면 반영 여부를 모른다고 알린다.
// 서버 오류(PgError)는 되돌려졌다는 뜻이고, 보내기 전에 실패했으면(SafeToRetry) 서버가 받지 않았다.
// 그 밖(연결 끊김·취소·시간 초과)은 서버가 이미 커밋한 뒤 응답만 잃었을 수 있다.
func outcomeUnknown(what string, err error) error {
	var pgErr *pgconn.PgError
	if errors.As(err, &pgErr) || pgconn.SafeToRetry(err) {
		return err
	}
	return fmt.Errorf("%s outcome unknown: %w; verify by re-querying", what, err)
}

// value 는 값 자리를 쓴다. nil 은 NULL 이다.
func (b *sqlBuilder) value(v *string) {
	if !b.inline {
		b.args = append(b.args, v)
		fmt.Fprintf(b, "$%d", len(b.args))
		return
	}
	if v == nil {
		b.WriteString("NULL")
		return
	}
	b.WriteString(QuoteLiteral(*v))
}

// UndoChanges 는 ApplyChanges 로 반영한 묶음을 되돌리는 묶음을 만든다(R50). ApplyChanges 로 그대로 실행해
// 한 트랜잭션·영향 행 1 규칙을 같이 타고, 미리 보기는 ChangesSQL 로 만든다. 거꾸로 된 순서로 되돌린다.
// 옛값 확인은 반영 뒤 행(Applied)으로 해서 제출 뒤 다른 사람이 고친 행은 덮지 않고 묶음째 실패한다.
//   - UPDATE → 바꾼 열을 Change.Old 로 되돌린다. 행은 반영 뒤 PK 로 찾는다(PK 를 고쳤어도 맞는다)
//   - INSERT → 넣은 행을 PK 로 지운다. PK 가 아닌 열이 넣은 그대로인지 확인한다
//   - DELETE → 지운 행 전체(생성 열 빼고)를 OVERRIDING SYSTEM VALUE 로 다시 넣는다
func UndoChanges(cs ChangeSet, applied *Applied) (ChangeSet, error) {
	if applied == nil || len(applied.Rows) != len(cs.Changes) {
		return ChangeSet{}, errors.New("applied rows do not match the change set; nothing to undo")
	}
	if applied.NoReturning {
		return ChangeSet{}, errors.New("RETURNING * was refused (insufficient privilege); changed rows are unknown, cannot undo")
	}
	if len(cs.PK) == 0 {
		return ChangeSet{}, errors.New("change set has no primary key")
	}
	undo := ChangeSet{Table: cs.Table, PK: cs.PK, Changes: make([]Change, 0, len(cs.Changes))}
	for i := len(cs.Changes) - 1; i >= 0; i-- {
		ch, post := cs.Changes[i], applied.Rows[i]
		if post == nil {
			return ChangeSet{}, fmt.Errorf("change %d has no returned row; cannot undo", i+1)
		}
		key := make(map[string]string, len(cs.PK))
		for _, k := range cs.PK {
			v := post[k]
			if v == nil {
				return ChangeSet{}, fmt.Errorf("returned row has no primary key column %q; cannot undo", k)
			}
			key[k] = *v
		}
		switch ch.Op {
		case ChangeUpdate:
			u := Change{Op: ChangeUpdate, Key: key, Values: map[string]*string{}, Old: map[string]*string{}}
			for col := range ch.Values {
				old, ok := ch.Old[col]
				if !ok {
					// 잘린 칸 등 읽은 값을 모르는 열은 되돌릴 값이 없다
					return ChangeSet{}, fmt.Errorf("old value of column %q is unknown; cannot undo", col)
				}
				u.Values[col], u.Old[col] = old, post[col]
			}
			undo.Changes = append(undo.Changes, u)
		case ChangeInsert:
			d := Change{Op: ChangeDelete, Key: key, Old: map[string]*string{}}
			for col, v := range post {
				if !slices.Contains(cs.PK, col) {
					d.Old[col] = v
				}
			}
			undo.Changes = append(undo.Changes, d)
		case ChangeDelete:
			ins := Change{Op: ChangeInsert, Values: map[string]*string{}}
			for col, v := range post {
				if !slices.Contains(applied.Generated, col) {
					ins.Values[col] = v
				}
			}
			undo.Changes = append(undo.Changes, ins)
			undo.Override = true
		default:
			return ChangeSet{}, fmt.Errorf("unknown change op %d", ch.Op)
		}
	}
	return undo, nil
}
