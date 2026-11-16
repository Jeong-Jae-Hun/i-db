// Package athena 는 Amazon Athena 읽기 전용 저장소다(명세 R60·R68). 트리는 데이터 카탈로그 AwsDataCatalog 의
// 데이터베이스 → 테이블 → 열이고, 편집기는 읽기 SQL 만 StartQueryExecution 으로 보낸 뒤 끝날 때까지 기다려 결과를 받는다.
package athena

import (
	"context"
	"errors"
	"fmt"
	"slices"
	"strings"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/service/athena"
	"github.com/aws/aws-sdk-go-v2/service/athena/types"
	"github.com/aws/smithy-go"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// Backend 는 Athena 접속 하나다. 자격 증명은 저장하지 않고 Profile(없으면 표준 체인)로 받는다(R60.1).
type Backend struct {
	cl        api
	workgroup string
	output    string
	// 상태 묻기 간격. 처음 짧게 묻고 두 배씩 늘려 maxPoll 에서 멈춘다. 시험이 줄인다
	minPoll, maxPoll time.Duration
}

// api 는 Backend 가 부르는 Athena 작업이다. 시험은 가짜로 바꿔 끼운다(네트워크 없음).
// StopQueryExecution 만 읽기가 아니다: 취소한 질의가 서버에서 계속 돌며 스캔 요금을 쌓지 않게 멈춘다(R68.3).
type api interface {
	athena.ListDatabasesAPIClient
	athena.ListTableMetadataAPIClient
	athena.GetQueryResultsAPIClient
	GetTableMetadata(context.Context, *athena.GetTableMetadataInput, ...func(*athena.Options)) (*athena.GetTableMetadataOutput, error)
	StartQueryExecution(context.Context, *athena.StartQueryExecutionInput, ...func(*athena.Options)) (*athena.StartQueryExecutionOutput, error)
	GetQueryExecution(context.Context, *athena.GetQueryExecutionInput, ...func(*athena.Options)) (*athena.GetQueryExecutionOutput, error)
	StopQueryExecution(context.Context, *athena.StopQueryExecutionInput, ...func(*athena.Options)) (*athena.StopQueryExecutionOutput, error)
	GetWorkGroup(context.Context, *athena.GetWorkGroupInput, ...func(*athena.Options)) (*athena.GetWorkGroupOutput, error)
}

const (
	catalog          = "AwsDataCatalog"
	defaultWorkgroup = "primary"
	// reuseMinutes 안에 같은 질의를 다시 보내면 Athena 가 이전 결과를 돌려주고 다시 스캔하지 않는다(요금 아낌)
	reuseMinutes = 60
	pageRows     = 1000 // GetQueryResults 한 번의 상한
	stopTimeout  = 5 * time.Second
)

// New 는 접속을 만든다. 실제 연결은 첫 호출 때 한다.
func New(ctx context.Context, c db.Conn) (source.Backend, error) {
	var opts []func(*config.LoadOptions) error
	if c.Region != "" {
		opts = append(opts, config.WithRegion(c.Region))
	}
	if c.Profile != "" {
		opts = append(opts, config.WithSharedConfigProfile(c.Profile))
	}
	cfg, err := config.LoadDefaultConfig(ctx, opts...)
	if err != nil {
		return nil, fmt.Errorf("load aws config: %w", err)
	}
	cl := athena.NewFromConfig(cfg, func(o *athena.Options) {
		o.HTTPClient = source.HTTPClient() // 리다이렉트를 따라가지 않는다
	})
	return newBackend(cl, c), nil
}

func newBackend(cl api, c db.Conn) *Backend {
	wg := c.Workgroup
	if wg == "" {
		wg = defaultWorkgroup
	}
	return &Backend{cl: cl, workgroup: wg, output: c.Output, minPoll: 200 * time.Millisecond, maxPoll: 2 * time.Second}
}

func (b *Backend) Close() error { return nil }

// ─── 트리 ───

// Children 은 "" 이면 데이터베이스, "<db>" 면 테이블(Info 는 테이블 종류), "<db>/<테이블>" 이면 열이다.
func (b *Backend) Children(ctx context.Context, path string) ([]source.Node, error) {
	if path == "" {
		var out []source.Node
		p := athena.NewListDatabasesPaginator(b.cl, &athena.ListDatabasesInput{CatalogName: aws.String(catalog)})
		for p.HasMorePages() {
			page, err := p.NextPage(ctx)
			if err != nil {
				return nil, wrap("ListDatabases", err)
			}
			for _, d := range page.DatabaseList {
				name := aws.ToString(d.Name)
				out = append(out, source.Node{Name: name, Path: name, HasChildren: true})
			}
		}
		return out, nil
	}
	dbName, table, isTable := strings.Cut(path, "/")
	if !isTable {
		var out []source.Node
		p := athena.NewListTableMetadataPaginator(b.cl, &athena.ListTableMetadataInput{CatalogName: aws.String(catalog), DatabaseName: aws.String(dbName)})
		for p.HasMorePages() {
			page, err := p.NextPage(ctx)
			if err != nil {
				return nil, wrap("ListTableMetadata", err)
			}
			for _, t := range page.TableMetadataList {
				name := aws.ToString(t.Name)
				out = append(out, source.Node{Name: name, Path: dbName + "/" + name, Info: aws.ToString(t.TableType), HasChildren: true, Openable: true})
			}
		}
		return out, nil
	}
	if strings.Contains(table, "/") { // 열 줄은 잎이다
		return nil, nil
	}
	md, err := b.table(ctx, dbName, table)
	if err != nil {
		return nil, err
	}
	return columnNodes(path, md), nil
}

// columnNodes 는 열(`이름 타입`) 다음에 파티션 키(`타입 (파티션)`)를 보인다.
func columnNodes(path string, md *types.TableMetadata) []source.Node {
	out := make([]source.Node, 0, len(md.Columns)+len(md.PartitionKeys))
	for _, c := range md.Columns {
		out = append(out, source.Node{Name: aws.ToString(c.Name), Path: path + "/" + aws.ToString(c.Name), Info: aws.ToString(c.Type)})
	}
	for _, c := range md.PartitionKeys {
		out = append(out, source.Node{Name: aws.ToString(c.Name), Path: path + "/" + aws.ToString(c.Name), Info: aws.ToString(c.Type) + " (파티션)"})
	}
	return out
}

func (b *Backend) table(ctx context.Context, dbName, table string) (*types.TableMetadata, error) {
	out, err := b.cl.GetTableMetadata(ctx, &athena.GetTableMetadataInput{CatalogName: aws.String(catalog),
		DatabaseName: aws.String(dbName), TableName: aws.String(table)})
	if err != nil {
		return nil, wrap("GetTableMetadata", err)
	}
	if out.TableMetadata == nil {
		return nil, fmt.Errorf("athena: table %s.%s has no metadata", dbName, table)
	}
	return out.TableMetadata, nil
}

// ─── 실행 ───

// OpenSQL 은 테이블 Enter 가 보내는 미리 보기 질의다(R68.1). path 는 "<db>/<테이블>" 이다.
func OpenSQL(path string, limit int) string {
	dbName, table, _ := strings.Cut(path, "/")
	return fmt.Sprintf("SELECT * FROM %s.%s LIMIT %d", quoteIdent(dbName), quoteIdent(table), limit)
}

func quoteIdent(s string) string { return `"` + strings.ReplaceAll(s, `"`, `""`) + `"` }

// Open 은 테이블 미리 보기다. LIMIT 이 있어도 Athena 는 읽은 파일만큼 요금을 매기고, 파티션 테이블은 파티션 조건 없이
// 여러 파티션을 열 수 있어 확인을 받는다(R68.1). 확인받은 통합은 OpenSQL 을 Force 로 Run 한다.
func (b *Backend) Open(ctx context.Context, path string, limit int) (*db.Result, error) {
	dbName, table, ok := strings.Cut(path, "/")
	if !ok || strings.Contains(table, "/") {
		return nil, fmt.Errorf("athena: not a table: %s", path)
	}
	md, err := b.table(ctx, dbName, table)
	if err != nil {
		return nil, err
	}
	if len(md.PartitionKeys) > 0 {
		return nil, source.NeedsConfirm(noPartition)
	}
	return b.Run(ctx, source.Query{Text: OpenSQL(path, limit), Limit: limit, Force: true})
}

// Run 은 읽기 SQL 한 문장만 보낸다(R68.2). 끝날 때까지 상태를 묻고 결과를 q.Limit(0 이면 1,000)행까지 받는다.
// ctx 가 취소되면 StopQueryExecution 으로 서버의 질의를 멈춘다. 결과 Text 는 스캔 크기·엔진 시간이다.
func (b *Backend) Run(ctx context.Context, q source.Query) (*db.Result, error) {
	dbName, text := databaseDirective(q.Text)
	stmt, words, err := readOnly(text)
	if err != nil {
		return nil, err
	}
	if reason := expensive(words); reason != "" && !q.Force {
		return nil, source.NeedsConfirm(reason)
	}
	limit := q.Limit
	if limit <= 0 {
		limit = db.MaxRows
	}
	start := time.Now()
	id, err := b.start(ctx, stmt, dbName)
	if err != nil {
		return nil, err
	}
	qe, err := b.wait(ctx, id)
	if err != nil {
		return nil, err
	}
	res, err := b.results(ctx, id, qe.StatementType, limit)
	if err != nil {
		return nil, err
	}
	res.Text = statsNote(qe.Statistics)
	res.Elapsed = time.Since(start)
	return res, nil
}

func (b *Backend) start(ctx context.Context, stmt, dbName string) (*string, error) {
	in := &athena.StartQueryExecutionInput{
		QueryString: aws.String(stmt),
		WorkGroup:   aws.String(b.workgroup),
		ResultReuseConfiguration: &types.ResultReuseConfiguration{ResultReuseByAgeConfiguration: &types.ResultReuseByAgeConfiguration{
			Enabled: true, MaxAgeInMinutes: aws.Int32(reuseMinutes)}},
	}
	if b.output != "" {
		in.ResultConfiguration = &types.ResultConfiguration{OutputLocation: aws.String(b.output)}
	}
	if dbName != "" {
		in.QueryExecutionContext = &types.QueryExecutionContext{Catalog: aws.String(catalog), Database: aws.String(dbName)}
	}
	out, err := b.cl.StartQueryExecution(ctx, in)
	if err != nil && reuseRejected(err) {
		// 결과 재사용은 엔진 v3 작업 그룹만 받는다. 거절하면 재사용 없이 한 번 더 보낸다
		in.ResultReuseConfiguration = nil
		out, err = b.cl.StartQueryExecution(ctx, in)
	}
	if err != nil {
		return nil, wrap("StartQueryExecution", err)
	}
	return out.QueryExecutionId, nil
}

func reuseRejected(err error) bool {
	var ae smithy.APIError
	return errors.As(err, &ae) && ae.ErrorCode() == "InvalidRequestException" && strings.Contains(strings.ToLower(ae.ErrorMessage()), "reuse")
}

// wait 는 질의가 끝날 때까지 GetQueryExecution 으로 묻는다. 간격은 minPoll 에서 두 배씩 maxPoll 까지 늘린다.
func (b *Backend) wait(ctx context.Context, id *string) (*types.QueryExecution, error) {
	delay := b.minPoll
	for {
		out, err := b.cl.GetQueryExecution(ctx, &athena.GetQueryExecutionInput{QueryExecutionId: id})
		if err != nil {
			if ctx.Err() != nil {
				b.stop(ctx, id)
				return nil, ctx.Err()
			}
			return nil, wrap("GetQueryExecution", err)
		}
		qe := out.QueryExecution
		if qe != nil && qe.Status != nil {
			switch qe.Status.State {
			case types.QueryExecutionStateSucceeded:
				return qe, nil
			case types.QueryExecutionStateFailed:
				return nil, fmt.Errorf("athena: query failed: %s", failReason(qe.Status))
			case types.QueryExecutionStateCancelled:
				return nil, errors.New("athena: query cancelled")
			}
		}
		t := time.NewTimer(delay)
		select {
		case <-ctx.Done():
			t.Stop()
			b.stop(ctx, id)
			return nil, ctx.Err()
		case <-t.C:
		}
		delay = min(delay*2, b.maxPoll)
	}
}

// stop 은 취소된 ctx 대신 새 기한으로 StopQueryExecution 을 보낸다. 실패해도 취소 결과는 그대로라 오류를 버린다.
func (b *Backend) stop(ctx context.Context, id *string) {
	sctx, cancel := context.WithTimeout(context.WithoutCancel(ctx), stopTimeout)
	defer cancel()
	_, _ = b.cl.StopQueryExecution(sctx, &athena.StopQueryExecutionInput{QueryExecutionId: id})
}

func failReason(s *types.QueryExecutionStatus) string {
	if s.AthenaError != nil && aws.ToString(s.AthenaError.ErrorMessage) != "" {
		return aws.ToString(s.AthenaError.ErrorMessage)
	}
	if r := aws.ToString(s.StateChangeReason); r != "" {
		return r
	}
	return "unknown reason"
}

// results 는 GetQueryResults 를 이어 limit+1 행(잘림 가르기)까지 받는다. SELECT(DML) 결과는 첫 페이지 첫 줄이
// 열 이름 머리라 뺀다. SHOW·DESCRIBE(DDL) 결과에는 머리 줄이 없다.
func (b *Backend) results(ctx context.Context, id *string, st types.StatementType, limit int) (*db.Result, error) {
	res := &db.Result{Cols: []string{}, Rows: [][]string{}}
	in := &athena.GetQueryResultsInput{QueryExecutionId: id}
	for first := true; ; first = false {
		want := limit - len(res.Rows) + 1
		if first {
			want++ // 머리 줄
		}
		in.MaxResults = aws.Int32(int32(min(max(want, 1), pageRows)))
		out, err := b.cl.GetQueryResults(ctx, in)
		if err != nil {
			return nil, wrap("GetQueryResults", err)
		}
		var rows []types.Row
		if rs := out.ResultSet; rs != nil {
			rows = rs.Rows
			if first {
				if rs.ResultSetMetadata != nil {
					for _, ci := range rs.ResultSetMetadata.ColumnInfo {
						res.Cols = append(res.Cols, aws.ToString(ci.Name))
						res.Types = append(res.Types, aws.ToString(ci.Type))
					}
				}
				if st == types.StatementTypeDml && len(rows) > 0 && slices.Equal(rowValues(rows[0]), res.Cols) {
					rows = rows[1:]
				}
				if len(res.Cols) == 0 && len(rows) > 0 {
					for i := range rows[0].Data {
						res.Cols = append(res.Cols, fmt.Sprintf("col%d", i+1))
					}
				}
			}
		}
		for _, r := range rows {
			row := make([]string, len(res.Cols))
			for i := range row {
				row[i] = "NULL"
				if i < len(r.Data) && r.Data[i].VarCharValue != nil {
					row[i] = db.CapCell(*r.Data[i].VarCharValue)
				}
			}
			res.Rows = append(res.Rows, row)
		}
		if len(res.Rows) > limit {
			res.Rows, res.Truncated = res.Rows[:limit], true
			return res, nil
		}
		if out.NextToken == nil {
			return res, nil
		}
		in.NextToken = out.NextToken
	}
}

func rowValues(r types.Row) []string {
	out := make([]string, len(r.Data))
	for i, d := range r.Data {
		out[i] = aws.ToString(d.VarCharValue)
	}
	return out
}

// statsNote 는 결과 위 한 줄이다. 예: `스캔 1.2GB · 엔진 0.6초`, 이전 결과를 다시 쓰면 ` · 이전 결과 재사용`.
func statsNote(s *types.QueryExecutionStatistics) string {
	if s == nil {
		return ""
	}
	note := fmt.Sprintf("스캔 %s · 엔진 %.1f초", humanBytes(aws.ToInt64(s.DataScannedInBytes)), float64(aws.ToInt64(s.EngineExecutionTimeInMillis))/1000)
	if s.ResultReuseInformation != nil && s.ResultReuseInformation.ReusedPreviousResult {
		note += " · 이전 결과 재사용"
	}
	return note
}

// humanBytes 는 1024 단위로 줄인다(예: 1.2GB). db 의 Redis 메모리 표기와 같은 꼴이다.
func humanBytes(n int64) string {
	if n < 1024 {
		return fmt.Sprintf("%dB", n)
	}
	f := float64(n)
	for _, u := range []string{"KB", "MB", "GB", "TB"} {
		f /= 1024
		if f < 1024 || u == "TB" {
			return fmt.Sprintf("%.1f%s", f, u)
		}
	}
	return ""
}

// wrap 은 AWS 오류를 작업 이름·코드·메시지로 줄인다. 요청 서명·자격 증명은 오류에 싣지 않는다.
func wrap(op string, err error) error {
	var ae smithy.APIError
	if errors.As(err, &ae) {
		return fmt.Errorf("athena %s: %s: %s", op, ae.ErrorCode(), ae.ErrorMessage())
	}
	return fmt.Errorf("athena %s: %w", op, err)
}
