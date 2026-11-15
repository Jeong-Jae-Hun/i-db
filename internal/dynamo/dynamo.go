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
// 테이블 Enter(Open)는 스캔이라 늘 source.ErrNeedsConfirm 을 돌려준다(R63.1). Open 에는 Force 가 없으므로
// 확인받은 통합은 OpenForce 로 다시 부른다. source 패키지에 맞춘 선택 인터페이스로 꺼내 쓴다:
//
//	if f, ok := b.(interface{ OpenForce(context.Context, string, int) (*db.Result, error) }); ok { … }
type Backend struct {
	cl *dynamodb.Client
}

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

// Open 은 테이블 미리 보기다. 스캔이라 확인 없이는 보내지 않는다(R63.1). 확인 뒤에는 OpenForce.
func (b *Backend) Open(_ context.Context, path string, _ int) (*db.Result, error) {
	return nil, fmt.Errorf("%w: scan of table %q", source.ErrNeedsConfirm, path)
}

// OpenForce 는 확인받은 테이블 미리 보기다. Scan 으로 앞 limit 개를 읽는다(0 이면 50).
func (b *Backend) OpenForce(ctx context.Context, path string, limit int) (*db.Result, error) {
	if limit <= 0 {
		limit = 50
	}
	start := time.Now()
	d, err := b.describe(ctx, path)
	if err != nil {
		return nil, err
	}
	var items []map[string]types.AttributeValue
	in := &dynamodb.ScanInput{TableName: aws.String(path)}
	for {
		in.Limit = aws.Int32(int32(limit - len(items) + 1)) // 하나 더 받아 잘림을 가른다
		out, err := b.cl.Scan(ctx, in)
		if err != nil {
			return nil, wrap("Scan", err)
		}
		items = append(items, out.Items...)
		if len(items) > limit || out.LastEvaluatedKey == nil {
			break
		}
		in.ExclusiveStartKey = out.LastEvaluatedKey
	}
	hash, rng := keyNames(d.KeySchema)
	res := toResult(items, limit, []string{hash, rng})
	res.Elapsed = time.Since(start)
	return res, nil
}

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
		return nil, fmt.Errorf("%w: no partition key condition (full scan)", source.ErrNeedsConfirm)
	}
	var items []map[string]types.AttributeValue
	in := &dynamodb.ExecuteStatementInput{Statement: aws.String(q.Text)}
	for {
		in.Limit = aws.Int32(int32(limit - len(items) + 1))
		out, err := b.cl.ExecuteStatement(ctx, in)
		if err != nil {
			return nil, wrap("ExecuteStatement", err)
		}
		items = append(items, out.Items...)
		if len(items) > limit || out.NextToken == nil {
			break
		}
		in.NextToken = out.NextToken
	}
	res := toResult(items, limit, keys)
	res.Elapsed = time.Since(start)
	return res, nil
}

func (b *Backend) describe(ctx context.Context, table string) (*types.TableDescription, error) {
	out, err := b.cl.DescribeTable(ctx, &dynamodb.DescribeTableInput{TableName: aws.String(table)})
	if err != nil {
		return nil, wrap("DescribeTable", err)
	}
	return out.Table, nil
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

