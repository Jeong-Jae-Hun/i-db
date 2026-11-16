// Package dynamo 는 DynamoDB 읽기 전용 저장소다(명세 R60·R63). 트리는 테이블 → 키·인덱스이고,
// 편집기는 PartiQL SELECT 만 ExecuteStatement 로 보낸다.
package dynamo

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb/types"
	"github.com/aws/smithy-go"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// Backend 는 DynamoDB 접속 하나다.
//
// 테이블 Enter(Open)는 스캔이라 늘 source.ErrNeedsConfirm 을 돌려준다(R63.1). 확인받은 통합은
// `SELECT * FROM "테이블"` 을 Force 로 Run 한다.
type Backend struct {
	cl api

	// tables 는 Run 이 읽은 DescribeTable 이다. 결과 정렬을 서버로 다시 읽을지(SortKey) 화면이 네트워크 없이 고른다
	mu     sync.Mutex
	tables map[string]*types.TableDescription
}

// api 는 Backend 가 부르는 DynamoDB 작업이다. 시험은 가짜로 페이지 이어 받기를 본다.
type api interface {
	dynamodb.ListTablesAPIClient
	DescribeTable(context.Context, *dynamodb.DescribeTableInput, ...func(*dynamodb.Options)) (*dynamodb.DescribeTableOutput, error)
	ExecuteStatement(context.Context, *dynamodb.ExecuteStatementInput, ...func(*dynamodb.Options)) (*dynamodb.ExecuteStatementOutput, error)
}

// Run 이 이어 받는 상한이다. 거르기(WHERE 의 키 밖 조건)가 있으면 한 페이지(1MB 읽기)에 맞는 항목이 거의 없어도
// NextToken 이 이어지므로, 행 상한만으로는 큰 테이블을 끝까지 읽는다. 페이지 수와 시간으로 함께 끊고 잘림으로 알린다.
// 더 읽어야 하면 키 조건으로 좁히는 것이 답이다. 상한을 접속 설정으로 올리는 것은 필요할 때 한다.
const (
	maxPages   = 50
	runTimeout = 60 * time.Second // ctx 에 기한이 없을 때만
)

// New 는 접속을 만든다. 자격 증명은 저장하지 않고 Profile(없으면 표준 체인)로 받는다(R60.1).
// URL 이 있으면 그 주소로 보낸다(DynamoDB Local 등). 실제 연결은 첫 호출 때 한다.
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
	cl := dynamodb.NewFromConfig(cfg, func(o *dynamodb.Options) {
		if c.URL != "" {
			o.BaseEndpoint = aws.String(c.URL)
		}
		o.HTTPClient = source.HTTPClient() // 리다이렉트를 따라가지 않는다(3xx 는 SDK 가 오류로 낸다)
	})
	return &Backend{cl: cl}, nil
}

func (b *Backend) Close() error { return nil }

// ─── 트리 ───

// Children 은 "" 이면 테이블 이름, "<테이블>" 이면 항목 수·키·인덱스다. 테이블마다 DescribeTable 을 부르지
// 않도록 항목 수는 펼칠 때 첫 줄로 보인다.
func (b *Backend) Children(ctx context.Context, path string) ([]source.Node, error) {
	if path == "" {
		var out []source.Node
		p := dynamodb.NewListTablesPaginator(b.cl, &dynamodb.ListTablesInput{})
		for p.HasMorePages() {
			page, err := p.NextPage(ctx)
			if err != nil {
				return nil, wrap("ListTables", err)
			}
			for _, t := range page.TableNames {
				out = append(out, source.Node{Name: t, Path: t, HasChildren: true, Openable: true})
			}
		}
		return out, nil
	}
	if strings.Contains(path, "/") { // 키·인덱스 줄은 잎이다. 테이블 이름에는 / 가 올 수 없다
		return nil, nil
	}
	d, err := b.describe(ctx, path)
	if err != nil {
		return nil, err
	}
	return tableNodes(d), nil
}

func tableNodes(d *types.TableDescription) []source.Node {
	table := aws.ToString(d.TableName)
	attrType := map[string]string{}
	for _, a := range d.AttributeDefinitions {
		attrType[aws.ToString(a.AttributeName)] = string(a.AttributeType)
	}
	out := []source.Node{{Name: "항목 수", Path: table + "/#items", Info: "≈" + strconv.FormatInt(aws.ToInt64(d.ItemCount), 10)}}
	for _, k := range d.KeySchema {
		role := "PK"
		if k.KeyType == types.KeyTypeRange {
			role = "SK"
		}
		name := aws.ToString(k.AttributeName)
		out = append(out, source.Node{Name: role + " " + name, Path: table + "/" + role, Info: attrType[name]})
	}
	for _, g := range d.GlobalSecondaryIndexes {
		out = append(out, indexNode(table, "gsi", aws.ToString(g.IndexName), g.KeySchema))
	}
	for _, l := range d.LocalSecondaryIndexes {
		out = append(out, indexNode(table, "lsi", aws.ToString(l.IndexName), l.KeySchema))
	}
	return out
}

func indexNode(table, kind, name string, ks []types.KeySchemaElement) source.Node {
	hash, rng := keyNames(ks)
	info := hash
	if rng != "" {
		info += "/" + rng
	}
	return source.Node{Name: kind + " " + name, Path: table + "/" + kind + "/" + name, Info: info}
}

// ─── 실행 ───

// Open 은 테이블 미리 보기다. 스캔이라 확인 없이는 보내지 않는다(R63.1).
func (b *Backend) Open(context.Context, string, int) (*db.Result, error) {
	return nil, source.NeedsConfirm(fullScan)
}

const fullScan = "전체 스캔"

// Run 은 PartiQL SELECT 만 보낸다. 다른 문장은 ErrWrite, 파티션 키 조건이 없으면 전체 스캔이라
// Force 없이는 ErrNeedsConfirm 이다(R63.2). NextToken 을 이어 q.Limit(0 이면 1,000)행까지 받는다.
func (b *Backend) Run(ctx context.Context, q source.Query) (*db.Result, error) {
	toks := tokenize(q.Text)
	if !isSelect(toks) {
		return nil, source.ErrWrite
	}
	limit := q.Limit
	if limit <= 0 {
		limit = db.MaxRows
	}
	start := time.Now()
	var keys []string
	table, index, ok := target(toks)
	if ok {
		d, err := b.describe(ctx, table)
		if err != nil {
			return nil, err
		}
		hash, rng := keyNames(d.KeySchema)
		keys = []string{hash, rng}
		pk := hash
		if index != "" {
			ih, ir, found := indexKeys(d, index)
			if !found {
				return nil, fmt.Errorf("dynamodb: index %q not found on table %q", index, table)
			}
			pk = ih
			keys = append(keys, ih, ir)
		}
		ok = hasKeyCondition(toks, pk)
	}
	if !ok && !q.Force {
		return nil, source.NeedsConfirm(fullScan)
	}
	if _, has := ctx.Deadline(); !has {
		var cancel context.CancelFunc
		ctx, cancel = context.WithTimeout(ctx, runTimeout)
		defer cancel()
	}
	items, stop, err := b.pages(ctx, q.Text, limit)
	if err != nil {
		return nil, err
	}
	res := toResult(items, limit, keys)
	if stop != "" {
		res.Truncated, res.Text = true, stop
	}
	res.Elapsed = time.Since(start)
	return res, nil
}

// pages 는 NextToken 을 이어 limit+1 개(잘림 가르기)까지 받는다. maxPages 나 ctx 기한에 닿으면 받은 데까지 돌려주고
// stop 에 까닭을 싣는다. 받은 것이 없는데 기한이 지났으면 오류다.
func (b *Backend) pages(ctx context.Context, stmt string, limit int) (items []map[string]types.AttributeValue, stop string, err error) {
	in := &dynamodb.ExecuteStatementInput{Statement: aws.String(stmt)}
	for page := 0; ; page++ {
		if page == maxPages {
			return items, fmt.Sprintf("페이지 %d개에서 멈춤 (키 조건으로 좁히세요)", maxPages), nil
		}
		in.Limit = aws.Int32(int32(limit - len(items) + 1))
		out, err := b.cl.ExecuteStatement(ctx, in)
		if err != nil {
			if len(items) > 0 && errors.Is(ctx.Err(), context.DeadlineExceeded) {
				return items, "시간 초과로 멈춤 (키 조건으로 좁히세요)", nil
			}
			return nil, "", wrap("ExecuteStatement", err)
		}
		items = append(items, out.Items...)
		if len(items) > limit || out.NextToken == nil {
			return items, "", nil
		}
		in.NextToken = out.NextToken
	}
}

func (b *Backend) describe(ctx context.Context, table string) (*types.TableDescription, error) {
	out, err := b.cl.DescribeTable(ctx, &dynamodb.DescribeTableInput{TableName: aws.String(table)})
	if err != nil {
		return nil, wrap("DescribeTable", err)
	}
	if out.Table != nil {
		b.mu.Lock()
		if b.tables == nil {
			b.tables = map[string]*types.TableDescription{}
		}
		b.tables[table] = out.Table
		b.mu.Unlock()
	}
	return out.Table, nil
}

// ─── 서버 정렬 (R63.3) ───

// SortKey 는 PartiQL SELECT text 의 결과를 서버에서 정렬할 수 있으면 그 정렬 키(인덱스를 쓰면 인덱스의 정렬 키)다.
// WHERE 가 파티션 키를 = 로 고정해야 한다(IN·OR 는 파티션이 여럿이라 ORDER BY 를 받지 않는다). 키는 Run 이 읽어 둔
// DescribeTable 을 써서 네트워크를 쓰지 않는다. 정렬할 수 없으면 sk 는 비고 reason 에 까닭(한국어)이 있다.
func (b *Backend) SortKey(text string) (sk, reason string) {
	toks := tokenize(text)
	table, index, ok := target(toks)
	if !isSelect(toks) || !ok {
		return "", "SELECT 가 아님"
	}
	b.mu.Lock()
	d := b.tables[table]
	b.mu.Unlock()
	if d == nil {
		return "", "키 정보 없음"
	}
	pk, sk := keyNames(d.KeySchema)
	if index != "" {
		if pk, sk, ok = indexKeys(d, index); !ok {
			return "", "키 정보 없음"
		}
	}
	switch {
	case sk == "":
		return "", "정렬 키 없는 테이블"
	case !keyCondition(toks, pk, false):
		return "", "파티션 키 = 조건 없음"
	}
	return sk, ""
}

// Sorted 는 SELECT text 의 맨 바깥 ORDER BY 를 `ORDER BY "<sk>" ASC|DESC` 로 바꾸거나 붙인 글이다(R63.3).
// SELECT 가 아니거나 LIMIT 이 있으면 ok 가 false 다.
//   - 예: `SELECT * FROM "t" WHERE pk = 'a' -- 메모`, "sk", true → `SELECT * FROM "t" WHERE pk = 'a' ORDER BY "sk" DESC`
func Sorted(text, sk string, desc bool) (string, bool) {
	return sortedStatement(tokenize(text), text, sk, desc)
}

func keyNames(ks []types.KeySchemaElement) (hash, rng string) {
	for _, k := range ks {
		if k.KeyType == types.KeyTypeHash {
			hash = aws.ToString(k.AttributeName)
		} else {
			rng = aws.ToString(k.AttributeName)
		}
	}
	return hash, rng
}

func indexKeys(d *types.TableDescription, index string) (hash, rng string, ok bool) {
	for _, g := range d.GlobalSecondaryIndexes {
		if aws.ToString(g.IndexName) == index {
			hash, rng = keyNames(g.KeySchema)
			return hash, rng, true
		}
	}
	for _, l := range d.LocalSecondaryIndexes {
		if aws.ToString(l.IndexName) == index {
			hash, rng = keyNames(l.KeySchema)
			return hash, rng, true
		}
	}
	return "", "", false
}

// wrap 은 AWS 오류를 작업 이름·코드·메시지로 줄인다. 요청 서명·자격 증명은 오류에 싣지 않는다.
func wrap(op string, err error) error {
	var ae smithy.APIError
	if errors.As(err, &ae) {
		return fmt.Errorf("dynamodb %s: %s: %s", op, ae.ErrorCode(), ae.ErrorMessage())
	}
	return fmt.Errorf("dynamodb %s: %w", op, err)
}

// ─── 결과 ───

// toResult 는 항목을 표로 편다. 열은 keys(테이블·인덱스 키, 항목에 있는 것만) 먼저, 나머지 속성 이름순이다.
// 없는 속성은 NULL 칸이다. Types 는 열마다 처음 본 DynamoDB 타입 글자다(S·N·B·BOOL·NULL·M·L·SS·NS·BS).
func toResult(items []map[string]types.AttributeValue, limit int, keys []string) *db.Result {
	res := &db.Result{Cols: []string{}, Rows: [][]string{}}
	if len(items) > limit {
		items, res.Truncated = items[:limit], true
	}
	seen := map[string]bool{}
	var rest []string
	for _, it := range items {
		for k := range it {
			if !seen[k] {
				seen[k] = true
				rest = append(rest, k)
			}
		}
	}
	for _, k := range keys {
		if k != "" && seen[k] && !slices.Contains(res.Cols, k) {
			res.Cols = append(res.Cols, k)
		}
	}
	rest = slices.DeleteFunc(rest, func(k string) bool { return slices.Contains(res.Cols, k) })
	slices.Sort(rest)
	res.Cols = append(res.Cols, rest...)
	res.Types = make([]string, len(res.Cols))
	for _, it := range items {
		row := make([]string, len(res.Cols))
		for i, c := range res.Cols {
			v, ok := it[c]
			if !ok {
				row[i] = "NULL"
				continue
			}
			typ, text := cell(v)
			if res.Types[i] == "" {
				res.Types[i] = typ
			}
			row[i] = db.CapCell(text)
		}
		res.Rows = append(res.Rows, row)
	}
	return res
}

// cell 은 값 하나의 타입 글자와 보일 글자다. 숫자는 원문, M·L·집합은 DynamoDB JSON 을 푼 JSON 이다(R63.2).
func cell(v types.AttributeValue) (typ, text string) {
	switch v := v.(type) {
	case *types.AttributeValueMemberS:
		return "S", v.Value
	case *types.AttributeValueMemberN:
		return "N", v.Value
	case *types.AttributeValueMemberBOOL:
		return "BOOL", strconv.FormatBool(v.Value)
	case *types.AttributeValueMemberNULL:
		return "NULL", "NULL"
	case *types.AttributeValueMemberB:
		return "B", base64.StdEncoding.EncodeToString(v.Value)
	case *types.AttributeValueMemberM:
		return "M", toJSON(v)
	case *types.AttributeValueMemberL:
		return "L", toJSON(v)
	case *types.AttributeValueMemberSS:
		return "SS", toJSON(v)
	case *types.AttributeValueMemberNS:
		return "NS", toJSON(v)
	case *types.AttributeValueMemberBS:
		return "BS", toJSON(v)
	}
	return "?", fmt.Sprintf("%T", v)
}

// plain 은 DynamoDB JSON 을 보통 JSON 값으로 푼다. 숫자는 json.Number 로 원문 그대로 둔다(정밀도 보존).
func plain(v types.AttributeValue) any {
	switch v := v.(type) {
	case *types.AttributeValueMemberS:
		return v.Value
	case *types.AttributeValueMemberN:
		return json.Number(v.Value)
	case *types.AttributeValueMemberBOOL:
		return v.Value
	case *types.AttributeValueMemberNULL:
		return nil
	case *types.AttributeValueMemberB:
		return v.Value // encoding/json 이 base64 로 쓴다
	case *types.AttributeValueMemberM:
		m := make(map[string]any, len(v.Value))
		for k, e := range v.Value {
			m[k] = plain(e)
		}
		return m
	case *types.AttributeValueMemberL:
		l := make([]any, len(v.Value))
		for i, e := range v.Value {
			l[i] = plain(e)
		}
		return l
	case *types.AttributeValueMemberSS:
		return v.Value
	case *types.AttributeValueMemberNS:
		l := make([]json.Number, len(v.Value))
		for i, n := range v.Value {
			l[i] = json.Number(n)
		}
		return l
	case *types.AttributeValueMemberBS:
		return v.Value
	}
	return nil
}

// toJSON 은 한 줄 JSON 이다. < > & 는 그대로 둔다(HTML 이 아니라 표 칸이다).
func toJSON(v types.AttributeValue) string {
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	if err := enc.Encode(plain(v)); err != nil {
		return fmt.Sprintf("<%v>", err)
	}
	return strings.TrimSuffix(buf.String(), "\n")
}
