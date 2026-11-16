package athena

import (
	"context"
	"errors"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/athena"
	"github.com/aws/aws-sdk-go-v2/service/athena/types"
	"github.com/aws/smithy-go"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── 가짜 Athena ───

// fakeAPI 는 네트워크 없이 Athena 작업을 흉내 낸다. 부르지 않을 작업은 내장한 nil api 로 패닉한다.
type fakeAPI struct {
	api
	dbs      [][]string
	tables   map[string][]types.TableMetadata
	meta     map[string]*types.TableMetadata
	startErr []error // StartQueryExecution 호출마다 차례로 돌려줄 오류
	started  []*athena.StartQueryExecutionInput
	states   []types.QueryExecutionState // GetQueryExecution 이 차례로 돌려줄 상태, 다 쓰면 마지막을 되풀이
	status   *types.QueryExecutionStatus // 끝 상태의 덧붙일 값(실패 까닭)
	stmtType types.StatementType
	stats    *types.QueryExecutionStatistics
	onGet    func(n int) // GetQueryExecution n 번째 부름 뒤
	getCalls int
	pages    []*types.ResultSet
	asked    []int32 // GetQueryResults 의 MaxResults
	stopped  []string
	stopCtx  error // StopQueryExecution 이 받은 ctx 의 오류
	wg       *types.WorkGroup
	wgErr    error
}

func (f *fakeAPI) ListDatabases(_ context.Context, in *athena.ListDatabasesInput, _ ...func(*athena.Options)) (*athena.ListDatabasesOutput, error) {
	i := 0
	if in.NextToken != nil {
		i = int((*in.NextToken)[0] - '0')
	}
	out := &athena.ListDatabasesOutput{}
	for _, n := range f.dbs[i] {
		out.DatabaseList = append(out.DatabaseList, types.Database{Name: aws.String(n)})
	}
	if i+1 < len(f.dbs) {
		out.NextToken = aws.String(string(rune('0' + i + 1)))
	}
	return out, nil
}

func (f *fakeAPI) ListTableMetadata(_ context.Context, in *athena.ListTableMetadataInput, _ ...func(*athena.Options)) (*athena.ListTableMetadataOutput, error) {
	return &athena.ListTableMetadataOutput{TableMetadataList: f.tables[aws.ToString(in.DatabaseName)]}, nil
}

func (f *fakeAPI) GetTableMetadata(_ context.Context, in *athena.GetTableMetadataInput, _ ...func(*athena.Options)) (*athena.GetTableMetadataOutput, error) {
	md, ok := f.meta[aws.ToString(in.DatabaseName)+"."+aws.ToString(in.TableName)]
	if !ok {
		return nil, &smithy.GenericAPIError{Code: "MetadataException", Message: "not found"}
	}
	return &athena.GetTableMetadataOutput{TableMetadata: md}, nil
}

func (f *fakeAPI) StartQueryExecution(_ context.Context, in *athena.StartQueryExecutionInput, _ ...func(*athena.Options)) (*athena.StartQueryExecutionOutput, error) {
	cp := *in
	f.started = append(f.started, &cp)
	if n := len(f.started); n <= len(f.startErr) && f.startErr[n-1] != nil {
		return nil, f.startErr[n-1]
	}
	return &athena.StartQueryExecutionOutput{QueryExecutionId: aws.String("q-1")}, nil
}

func (f *fakeAPI) GetQueryExecution(ctx context.Context, in *athena.GetQueryExecutionInput, _ ...func(*athena.Options)) (*athena.GetQueryExecutionOutput, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	f.getCalls++
	state := f.states[min(f.getCalls, len(f.states))-1]
	st := &types.QueryExecutionStatus{State: state}
	if f.status != nil {
		st.AthenaError, st.StateChangeReason = f.status.AthenaError, f.status.StateChangeReason
	}
	if f.onGet != nil {
		f.onGet(f.getCalls)
	}
	return &athena.GetQueryExecutionOutput{QueryExecution: &types.QueryExecution{QueryExecutionId: in.QueryExecutionId,
		Status: st, StatementType: f.stmtType, Statistics: f.stats}}, nil
}

func (f *fakeAPI) GetQueryResults(_ context.Context, in *athena.GetQueryResultsInput, _ ...func(*athena.Options)) (*athena.GetQueryResultsOutput, error) {
	f.asked = append(f.asked, aws.ToInt32(in.MaxResults))
	i := len(f.asked) - 1
	out := &athena.GetQueryResultsOutput{ResultSet: f.pages[i]}
	if i+1 < len(f.pages) {
		out.NextToken = aws.String("next")
	}
	return out, nil
}

func (f *fakeAPI) StopQueryExecution(ctx context.Context, in *athena.StopQueryExecutionInput, _ ...func(*athena.Options)) (*athena.StopQueryExecutionOutput, error) {
	f.stopped = append(f.stopped, aws.ToString(in.QueryExecutionId))
	f.stopCtx = ctx.Err()
	return &athena.StopQueryExecutionOutput{}, nil
}

func (f *fakeAPI) GetWorkGroup(_ context.Context, in *athena.GetWorkGroupInput, _ ...func(*athena.Options)) (*athena.GetWorkGroupOutput, error) {
	if f.wgErr != nil {
		return nil, f.wgErr
	}
	return &athena.GetWorkGroupOutput{WorkGroup: f.wg}, nil
}

func testBackend(f *fakeAPI, c db.Conn) *Backend {
	b := newBackend(f, c)
	b.minPoll, b.maxPoll = time.Millisecond, 2*time.Millisecond
	return b
}

func row(vals ...string) types.Row {
	r := types.Row{}
	for _, v := range vals {
		if v == "<nil>" {
			r.Data = append(r.Data, types.Datum{})
			continue
		}
		r.Data = append(r.Data, types.Datum{VarCharValue: aws.String(v)})
	}
	return r
}

func meta(cols ...string) *types.ResultSetMetadata {
	m := &types.ResultSetMetadata{}
	for _, c := range cols {
		name, typ, _ := strings.Cut(c, " ")
		m.ColumnInfo = append(m.ColumnInfo, types.ColumnInfo{Name: aws.String(name), Type: aws.String(typ)})
	}
	return m
}

func succeeded() *fakeAPI {
	return &fakeAPI{states: []types.QueryExecutionState{types.QueryExecutionStateSucceeded}, stmtType: types.StatementTypeDml}
}

// ─── 읽기 판정 ───

func TestReadOnly_읽기_문장만_받고_쓰기와_여러_문장은_ErrWrite(t *testing.T) {
	ok := []string{
		`SELECT * FROM "db"."t" LIMIT 10`,
		"select 'delete from t' as s, \"update\" from t where a = 1;",
		"-- 설명\nWITH x AS (SELECT 1 a) SELECT a FROM x LIMIT 1",
		"SHOW CREATE TABLE db.t",
		"SHOW TABLES IN db",
		"DESCRIBE db.t",
		"EXPLAIN SELECT 1",
		"/* 앞 */ select 1 limit 1 ; ",
	}
	for _, q := range ok {
		if _, _, err := readOnly(q); err != nil {
			t.Errorf("readOnly(%q) = %v, want ok", q, err)
		}
	}
	bad := []string{
		"INSERT INTO t SELECT * FROM s",
		"CREATE TABLE t2 AS SELECT * FROM t",
		"UNLOAD (SELECT * FROM t) TO 's3://b/p/' WITH (format = 'PARQUET')",
		"DROP TABLE t",
		"ALTER TABLE t ADD PARTITION (dt='2026-01-01')",
		"MSCK REPAIR TABLE t",
		"WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x",
		"EXPLAIN ANALYZE INSERT INTO t VALUES (1)",
		"SELECT * FROM (DELETE FROM t)",
		"SELECT 1; SELECT 2",
		"SELECT 1; DROP TABLE t",
		"VACUUM t",
		"",
	}
	for _, q := range bad {
		if _, _, err := readOnly(q); !errors.Is(err, source.ErrWrite) {
			t.Errorf("readOnly(%q) = %v, want ErrWrite", q, err)
		}
	}
}

func TestRun_쓰기_문장은_Athena_를_부르지_않는다(t *testing.T) {
	f := succeeded()
	b := testBackend(f, db.Conn{})
	for _, q := range []string{"DROP TABLE t", "CREATE TABLE x AS SELECT 1", "SELECT 1; SELECT 2"} {
		if _, err := b.Run(context.Background(), source.Query{Text: q, Force: true}); !errors.Is(err, source.ErrWrite) {
			t.Errorf("Run(%q) err = %v, want ErrWrite", q, err)
		}
	}
	if len(f.started) != 0 {
		t.Errorf("쓰기 문장이 StartQueryExecution 으로 갔다: %d번", len(f.started))
	}
}

func TestRun_LIMIT_도_WHERE_도_없는_SELECT_는_전체_스캔_확인을_받는다(t *testing.T) {
	f := succeeded()
	f.pages = []*types.ResultSet{{ResultSetMetadata: meta("a"), Rows: []types.Row{row("a")}}}
	b := testBackend(f, db.Conn{})
	_, err := b.Run(context.Background(), source.Query{Text: `SELECT * FROM "db"."t"`})
	if !errors.Is(err, source.ErrNeedsConfirm) || source.ConfirmReason(err) != fullScan || len(f.started) != 0 {
		t.Fatalf("err = %v (까닭 %q), started %d, want 전체 스캔 확인·보내지 않음", err, source.ConfirmReason(err), len(f.started))
	}
	for _, q := range []string{"SELECT * FROM t LIMIT 5", "SELECT * FROM t WHERE dt = '2026-01-01'", "SHOW TABLES", "EXPLAIN SELECT * FROM t"} {
		if reason := expensive(mustWords(t, q)); reason != "" {
			t.Errorf("expensive(%q) = %q, want 없음", q, reason)
		}
	}
	if reason := expensive(mustWords(t, "EXPLAIN ANALYZE SELECT * FROM t")); reason != fullScan {
		t.Errorf("EXPLAIN ANALYZE 는 실제로 돌아 확인해야 한다: %q", reason)
	}
	if _, err := b.Run(context.Background(), source.Query{Text: `SELECT * FROM "db"."t"`, Force: true}); err != nil || len(f.started) != 1 {
		t.Fatalf("Force 면 보내야 한다: err %v, started %d", err, len(f.started))
	}
}

func mustWords(t *testing.T, q string) []string {
	t.Helper()
	_, words, err := readOnly(q)
	if err != nil {
		t.Fatal(err)
	}
	return words
}

// ─── 실행 ───

func TestRun_작업_그룹_결과_위치_데이터베이스_주석_재사용을_실어_보낸다(t *testing.T) {
	f := succeeded()
	f.pages = []*types.ResultSet{{ResultSetMetadata: meta("a")}, {ResultSetMetadata: meta("a")}}
	b := testBackend(f, db.Conn{Output: "s3://bucket/prefix/"})
	if _, err := b.Run(context.Background(), source.Query{Text: "-- database: sales\nSELECT 1 LIMIT 1"}); err != nil {
		t.Fatal(err)
	}
	in := f.started[0]
	if aws.ToString(in.WorkGroup) != "primary" || aws.ToString(in.QueryString) != "SELECT 1 LIMIT 1" {
		t.Errorf("WorkGroup %q QueryString %q, want primary·주석 줄을 뗀 글", aws.ToString(in.WorkGroup), aws.ToString(in.QueryString))
	}
	if in.ResultConfiguration == nil || aws.ToString(in.ResultConfiguration.OutputLocation) != "s3://bucket/prefix/" {
		t.Errorf("ResultConfiguration = %+v", in.ResultConfiguration)
	}
	if c := in.QueryExecutionContext; c == nil || aws.ToString(c.Database) != "sales" || aws.ToString(c.Catalog) != catalog {
		t.Errorf("QueryExecutionContext = %+v", c)
	}
	if r := in.ResultReuseConfiguration; r == nil || !r.ResultReuseByAgeConfiguration.Enabled || aws.ToInt32(r.ResultReuseByAgeConfiguration.MaxAgeInMinutes) != 60 {
		t.Errorf("ResultReuseConfiguration = %+v", r)
	}

	// 작업 그룹을 주고 결과 위치·주석이 없으면 둘 다 빼고 작업 그룹 설정을 따른다
	f2 := succeeded()
	f2.pages = f.pages
	if _, err := testBackend(f2, db.Conn{Workgroup: "analysts"}).Run(context.Background(), source.Query{Text: "SELECT 1 LIMIT 1"}); err != nil {
		t.Fatal(err)
	}
	if in := f2.started[0]; aws.ToString(in.WorkGroup) != "analysts" || in.ResultConfiguration != nil || in.QueryExecutionContext != nil {
		t.Errorf("in = WorkGroup %q ResultConfiguration %+v Context %+v", aws.ToString(in.WorkGroup), in.ResultConfiguration, in.QueryExecutionContext)
	}
}

func TestRun_결과_재사용을_거절하면_재사용_없이_한_번_더_보낸다(t *testing.T) {
	f := succeeded()
	f.pages = []*types.ResultSet{{ResultSetMetadata: meta("a")}}
	f.startErr = []error{&smithy.GenericAPIError{Code: "InvalidRequestException", Message: "Result reuse is not supported for engine version 2"}}
	if _, err := testBackend(f, db.Conn{}).Run(context.Background(), source.Query{Text: "SELECT 1 LIMIT 1"}); err != nil {
		t.Fatal(err)
	}
	if len(f.started) != 2 || f.started[1].ResultReuseConfiguration != nil {
		t.Fatalf("started %d, 두 번째 재사용 %+v", len(f.started), f.started[len(f.started)-1].ResultReuseConfiguration)
	}

	// 다른 오류(결과 위치 없음)는 다시 보내지 않고 그대로 보인다
	f2 := succeeded()
	f2.startErr = []error{&smithy.GenericAPIError{Code: "InvalidRequestException", Message: "No output location provided."}}
	_, err := testBackend(f2, db.Conn{}).Run(context.Background(), source.Query{Text: "SELECT 1 LIMIT 1"})
	if err == nil || err.Error() != "athena StartQueryExecution: InvalidRequestException: No output location provided." || len(f2.started) != 1 {
		t.Fatalf("err = %v, started %d", err, len(f2.started))
	}
}

func TestRun_SELECT_결과는_첫_줄_머리를_빼고_SHOW_결과는_그대로_둔다(t *testing.T) {
	f := succeeded()
	f.stats = &types.QueryExecutionStatistics{DataScannedInBytes: aws.Int64(1288490189), EngineExecutionTimeInMillis: aws.Int64(612)}
	f.pages = []*types.ResultSet{{ResultSetMetadata: meta("id bigint", "name varchar"),
		Rows: []types.Row{row("id", "name"), row("1", "가"), row("2", "<nil>")}}}
	res, err := testBackend(f, db.Conn{}).Run(context.Background(), source.Query{Text: "SELECT id, name FROM t LIMIT 2"})
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(res.Cols, []string{"id", "name"}) || !reflect.DeepEqual(res.Types, []string{"bigint", "varchar"}) {
		t.Errorf("Cols %v Types %v", res.Cols, res.Types)
	}
	if want := [][]string{{"1", "가"}, {"2", "NULL"}}; !reflect.DeepEqual(res.Rows, want) || res.Truncated {
		t.Errorf("Rows = %v truncated %v, want %v", res.Rows, res.Truncated, want)
	}
	if res.Text != "스캔 1.2GB · 엔진 0.6초" {
		t.Errorf("Text = %q", res.Text)
	}

	ddl := &fakeAPI{states: []types.QueryExecutionState{types.QueryExecutionStateSucceeded}, stmtType: types.StatementTypeDdl,
		pages: []*types.ResultSet{{ResultSetMetadata: meta("tab_name string"), Rows: []types.Row{row("tab_name"), row("orders")}}}}
	res, err = testBackend(ddl, db.Conn{}).Run(context.Background(), source.Query{Text: "SHOW TABLES"})
	if err != nil {
		t.Fatal(err)
	}
	if want := [][]string{{"tab_name"}, {"orders"}}; !reflect.DeepEqual(res.Rows, want) {
		t.Errorf("DDL 결과에는 머리 줄이 없어 그대로 둬야 한다: %v", res.Rows)
	}
}

func TestRun_페이지를_이어_받되_한도에서_자르고_더_묻지_않는다(t *testing.T) {
	f := succeeded()
	f.pages = []*types.ResultSet{
		{ResultSetMetadata: meta("n"), Rows: []types.Row{row("n"), row("1"), row("2")}},
		{Rows: []types.Row{row("3"), row("4")}},
		{Rows: []types.Row{row("5")}},
	}
	res, err := testBackend(f, db.Conn{}).Run(context.Background(), source.Query{Text: "SELECT n FROM t LIMIT 100", Limit: 3})
	if err != nil {
		t.Fatal(err)
	}
	if want := [][]string{{"1"}, {"2"}, {"3"}}; !reflect.DeepEqual(res.Rows, want) || !res.Truncated {
		t.Errorf("Rows = %v truncated %v, want %v·잘림", res.Rows, res.Truncated, want)
	}
	// 첫 요청은 한도+잘림 가르기 1+머리 1, 다음은 남은 만큼+1 이다. 한도를 넘으면 세 번째 페이지는 묻지 않는다
	if want := []int32{5, 2}; !reflect.DeepEqual(f.asked, want) {
		t.Errorf("MaxResults = %v, want %v", f.asked, want)
	}

	big := succeeded()
	big.pages = []*types.ResultSet{{ResultSetMetadata: meta("n")}}
	if _, err := testBackend(big, db.Conn{}).Run(context.Background(), source.Query{Text: "SELECT 1 LIMIT 1", Limit: 5000}); err != nil {
		t.Fatal(err)
	}
	if big.asked[0] != pageRows {
		t.Errorf("MaxResults 는 Athena 상한 %d 을 넘지 않아야 한다: %d", pageRows, big.asked[0])
	}
}

func TestRun_실패한_질의는_Athena_까닭을_보인다(t *testing.T) {
	f := &fakeAPI{states: []types.QueryExecutionState{types.QueryExecutionStateQueued, types.QueryExecutionStateRunning, types.QueryExecutionStateFailed},
		status: &types.QueryExecutionStatus{AthenaError: &types.AthenaError{ErrorMessage: aws.String("TABLE_NOT_FOUND: line 1:15: Table 'x' does not exist")}}}
	_, err := testBackend(f, db.Conn{}).Run(context.Background(), source.Query{Text: "SELECT * FROM x LIMIT 1"})
	if err == nil || err.Error() != "athena: query failed: TABLE_NOT_FOUND: line 1:15: Table 'x' does not exist" || f.getCalls != 3 {
		t.Fatalf("err = %v, getCalls %d", err, f.getCalls)
	}
}

func TestRun_취소하면_StopQueryExecution_으로_서버_질의를_멈춘다(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	f := &fakeAPI{states: []types.QueryExecutionState{types.QueryExecutionStateRunning}}
	f.onGet = func(n int) {
		if n == 2 {
			cancel()
		}
	}
	_, err := testBackend(f, db.Conn{}).Run(ctx, source.Query{Text: "SELECT * FROM t LIMIT 1"})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("err = %v, want context.Canceled", err)
	}
	if !reflect.DeepEqual(f.stopped, []string{"q-1"}) || f.stopCtx != nil {
		t.Fatalf("stopped %v (ctx 오류 %v), want [q-1]·살아 있는 ctx", f.stopped, f.stopCtx)
	}
	if len(f.asked) != 0 {
		t.Error("취소한 뒤 결과를 묻지 않아야 한다")
	}
}

// ─── 트리·미리 보기 ───

// childNames 는 path 의 자식을 `이름|경로|Info` 줄로 편다.
func childNames(t *testing.T, b *Backend, path string) []string {
	t.Helper()
	nodes, err := b.Children(context.Background(), path)
	if err != nil {
		t.Fatal(err)
	}
	var out []string
	for _, n := range nodes {
		out = append(out, n.Name+"|"+n.Path+"|"+n.Info)
	}
	return out
}

func TestChildren_데이터베이스_테이블_열과_파티션_키를_보인다(t *testing.T) {
	f := &fakeAPI{
		dbs: [][]string{{"logs"}, {"sales"}},
		tables: map[string][]types.TableMetadata{"sales": {
			{Name: aws.String("orders"), TableType: aws.String("EXTERNAL_TABLE")},
			{Name: aws.String("v_daily"), TableType: aws.String("VIRTUAL_VIEW")},
		}},
		meta: map[string]*types.TableMetadata{"sales.orders": {
			Columns:       []types.Column{{Name: aws.String("id"), Type: aws.String("bigint")}},
			PartitionKeys: []types.Column{{Name: aws.String("dt"), Type: aws.String("string")}},
		}},
	}
	b := testBackend(f, db.Conn{})
	if got, want := childNames(t, b, ""), []string{"logs|logs|", "sales|sales|"}; !reflect.DeepEqual(got, want) {
		t.Errorf("데이터베이스 = %v, want %v (페이지 이어 받기)", got, want)
	}
	if got, want := childNames(t, b, "sales"), []string{"orders|sales/orders|EXTERNAL_TABLE", "v_daily|sales/v_daily|VIRTUAL_VIEW"}; !reflect.DeepEqual(got, want) {
		t.Errorf("테이블 = %v, want %v", got, want)
	}
	if got, want := childNames(t, b, "sales/orders"), []string{"id|sales/orders/id|bigint", "dt|sales/orders/dt|string (파티션)"}; !reflect.DeepEqual(got, want) {
		t.Errorf("열 = %v, want %v", got, want)
	}
	if got := childNames(t, b, "sales/orders/id"); got != nil {
		t.Errorf("열은 잎이어야 한다: %v", got)
	}
}

func TestOpen_파티션_테이블은_확인을_받고_아니면_LIMIT_50_으로_읽는다(t *testing.T) {
	f := succeeded()
	f.meta = map[string]*types.TableMetadata{
		"sales.orders": {PartitionKeys: []types.Column{{Name: aws.String("dt")}}},
		"sales.users":  {},
	}
	f.pages = []*types.ResultSet{{ResultSetMetadata: meta("id"), Rows: []types.Row{row("id"), row("7")}}}
	b := testBackend(f, db.Conn{})
	_, err := b.Open(context.Background(), "sales/orders", 50)
	if source.ConfirmReason(err) != noPartition || len(f.started) != 0 {
		t.Fatalf("err = %v, started %d, want 파티션 조건 없음 확인", err, len(f.started))
	}
	res, err := b.Open(context.Background(), "sales/users", 50)
	if err != nil || len(res.Rows) != 1 {
		t.Fatalf("res %+v err %v", res, err)
	}
	if got := aws.ToString(f.started[0].QueryString); got != `SELECT * FROM "sales"."users" LIMIT 50` {
		t.Errorf("QueryString = %q", got)
	}
	if got := OpenSQL(`we"ird/t`, 50); got != `SELECT * FROM "we""ird"."t" LIMIT 50` {
		t.Errorf("OpenSQL 은 따옴표를 겹쳐야 한다: %q", got)
	}
}

func TestDatabaseDirective_첫_줄_주석만_데이터베이스로_읽는다(t *testing.T) {
	cases := []struct{ in, db, rest string }{
		{"-- database: sales\nSELECT 1", "sales", "SELECT 1"},
		{"\n  --DATABASE: \"logs\"\r\nSHOW TABLES", "logs", "SHOW TABLES"},
		{"SELECT 1\n-- database: sales", "", "SELECT 1\n-- database: sales"},
		{"-- 그냥 주석\nSELECT 1", "", "-- 그냥 주석\nSELECT 1"},
	}
	for _, c := range cases {
		gotDB, gotRest := databaseDirective(c.in)
		if gotDB != c.db || gotRest != c.rest {
			t.Errorf("databaseDirective(%q) = %q %q, want %q %q", c.in, gotDB, gotRest, c.db, c.rest)
		}
	}
}

// ─── 대시보드 ───

func TestProbe_작업_그룹_상태와_스캔_상한_엔진_판을_보인다(t *testing.T) {
	f := &fakeAPI{wg: &types.WorkGroup{State: types.WorkGroupStateEnabled, Configuration: &types.WorkGroupConfiguration{
		BytesScannedCutoffPerQuery: aws.Int64(10 << 30),
		EngineVersion:              &types.EngineVersion{EffectiveEngineVersion: aws.String("Athena engine version 3")}}}}
	h := testBackend(f, db.Conn{Workgroup: "analysts"}).Probe(context.Background())
	if !h.OK || h.Summary() != "ENABLED · 스캔 상한 10.0GB" || h.Version != "Athena engine version 3" || h.Metrics[0].Warn {
		t.Fatalf("Probe = %+v", h)
	}

	off := &fakeAPI{wg: &types.WorkGroup{State: types.WorkGroupStateDisabled}}
	if h := testBackend(off, db.Conn{}).Probe(context.Background()); h.Summary() != "DISABLED" || !h.Metrics[0].Warn {
		t.Errorf("꺼진 작업 그룹은 경고여야 한다: %+v", h)
	}

	bad := &fakeAPI{wgErr: &smithy.GenericAPIError{Code: "AccessDeniedException", Message: "not authorized"}}
	if h := testBackend(bad, db.Conn{}).Probe(context.Background()); h.OK || h.Err != "athena GetWorkGroup: AccessDeniedException: not authorized" {
		t.Errorf("Probe = %+v", h)
	}
}
