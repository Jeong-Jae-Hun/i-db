package dynamo

import (
	"context"
	"errors"
	"fmt"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb/types"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

func TestCell_타입마다_보일_글자로_푼다(t *testing.T) {
	cases := []struct {
		v             types.AttributeValue
		typ, wantText string
	}{
		{&types.AttributeValueMemberS{Value: "안녕"}, "S", "안녕"},
		{&types.AttributeValueMemberN{Value: "12345678901234567890.10"}, "N", "12345678901234567890.10"},
		{&types.AttributeValueMemberBOOL{Value: true}, "BOOL", "true"},
		{&types.AttributeValueMemberBOOL{Value: false}, "BOOL", "false"},
		{&types.AttributeValueMemberNULL{Value: true}, "NULL", "NULL"},
		{&types.AttributeValueMemberB{Value: []byte("hi")}, "B", "aGk="},
		{&types.AttributeValueMemberM{Value: map[string]types.AttributeValue{
			"b": &types.AttributeValueMemberN{Value: "1.50"},
			"a": &types.AttributeValueMemberS{Value: "<x&y>"},
			"n": &types.AttributeValueMemberNULL{Value: true},
		}}, "M", `{"a":"<x&y>","b":1.50,"n":null}`},
		{&types.AttributeValueMemberL{Value: []types.AttributeValue{
			&types.AttributeValueMemberS{Value: "a"},
			&types.AttributeValueMemberBOOL{Value: true},
			&types.AttributeValueMemberL{Value: []types.AttributeValue{&types.AttributeValueMemberN{Value: "2"}}},
			&types.AttributeValueMemberB{Value: []byte("hi")},
		}}, "L", `["a",true,[2],"aGk="]`},
		{&types.AttributeValueMemberSS{Value: []string{"x", "y"}}, "SS", `["x","y"]`},
		{&types.AttributeValueMemberNS{Value: []string{"1", "2.0"}}, "NS", `[1,2.0]`},
		{&types.AttributeValueMemberBS{Value: [][]byte{[]byte("hi")}}, "BS", `["aGk="]`},
	}
	for _, c := range cases {
		typ, text := cell(c.v)
		if typ != c.typ || text != c.wantText {
			t.Errorf("cell(%T) = %q %q, want %q %q", c.v, typ, text, c.typ, c.wantText)
		}
	}
}

func TestToResult_키_열_먼저_나머지는_이름순이고_없는_속성은_NULL(t *testing.T) {
	items := []map[string]types.AttributeValue{
		{"zeta": &types.AttributeValueMemberS{Value: "z"}, "sk": &types.AttributeValueMemberN{Value: "1"},
			"pk": &types.AttributeValueMemberS{Value: "a"}, "alpha": &types.AttributeValueMemberN{Value: "3"}},
		{"pk": &types.AttributeValueMemberS{Value: "b"}, "sk": &types.AttributeValueMemberN{Value: "2"},
			"alpha": &types.AttributeValueMemberS{Value: "문자"}, "gsiPK": &types.AttributeValueMemberS{Value: "g"}},
	}
	res := toResult(items, 10, []string{"pk", "sk", "gsiPK", ""})
	if want := []string{"pk", "sk", "gsiPK", "alpha", "zeta"}; !reflect.DeepEqual(res.Cols, want) {
		t.Fatalf("Cols = %v, want %v", res.Cols, want)
	}
	if want := []string{"S", "N", "S", "N", "S"}; !reflect.DeepEqual(res.Types, want) {
		t.Errorf("Types = %v, want %v (열마다 처음 본 타입)", res.Types, want)
	}
	want := [][]string{{"a", "1", "NULL", "3", "z"}, {"b", "2", "g", "문자", "NULL"}}
	if !reflect.DeepEqual(res.Rows, want) || res.Truncated {
		t.Errorf("Rows = %v truncated=%v, want %v", res.Rows, res.Truncated, want)
	}
}

func TestToResult_항목에_없는_키_열은_빼고_한도를_넘으면_자른다(t *testing.T) {
	items := []map[string]types.AttributeValue{
		{"a": &types.AttributeValueMemberS{Value: "1"}},
		{"a": &types.AttributeValueMemberS{Value: "2"}},
		{"a": &types.AttributeValueMemberS{Value: "3"}},
	}
	res := toResult(items, 2, []string{"pk", "sk"})
	if !reflect.DeepEqual(res.Cols, []string{"a"}) || len(res.Rows) != 2 || !res.Truncated {
		t.Errorf("Cols=%v rows=%d truncated=%v, want [a] 2 true", res.Cols, len(res.Rows), res.Truncated)
	}
	empty := toResult(nil, 10, []string{"pk"})
	if empty.Cols == nil || empty.Rows == nil || len(empty.Cols) != 0 {
		t.Errorf("빈 결과는 빈 표여야 한다: %+v", empty)
	}
}

func TestCapText_큰_칸은_db_규칙대로_자른다(t *testing.T) {
	big := strings.Repeat("가", db.MaxCellBytes)
	got := db.CapCell(big)
	if !db.IsCappedCell(got) {
		t.Errorf("잘린 칸을 db.IsCappedCell 이 알아봐야 한다: 꼬리 %q", got[len(got)-30:])
	}
	if db.CapCell("짧음") != "짧음" {
		t.Error("짧은 칸은 그대로여야 한다")
	}
}

// Run 은 AWS 를 부르기 전에 쓰기 문장을 거른다. 클라이언트 없이도 ErrWrite 가 나와야 한다.
func TestRun_쓰기_문장은_보내지_않고_ErrWrite(t *testing.T) {
	b := &Backend{}
	for _, q := range []string{`DELETE FROM "t" WHERE pk='a'`, `UPDATE t SET a=1`, `INSERT INTO t VALUE {'pk':'a'}`} {
		if _, err := b.Run(context.Background(), source.Query{Text: q, Force: true}); !errors.Is(err, source.ErrWrite) {
			t.Errorf("Run(%q) err = %v, want ErrWrite", q, err)
		}
	}
}

func TestOpen_스캔이라_늘_확인을_받는다(t *testing.T) {
	if _, err := (&Backend{}).Open(context.Background(), "t", 50); !errors.Is(err, source.ErrNeedsConfirm) {
		t.Errorf("Open err = %v, want ErrNeedsConfirm", err)
	}
}

func TestTableNodes_항목_수_키_인덱스를_보인다(t *testing.T) {
	ks := func(h, r string) []types.KeySchemaElement {
		out := []types.KeySchemaElement{{AttributeName: aws.String(h), KeyType: types.KeyTypeHash}}
		if r != "" {
			out = append(out, types.KeySchemaElement{AttributeName: aws.String(r), KeyType: types.KeyTypeRange})
		}
		return out
	}
	d := &types.TableDescription{
		TableName: aws.String("orders"), ItemCount: aws.Int64(42), KeySchema: ks("pk", "sk"),
		AttributeDefinitions: []types.AttributeDefinition{
			{AttributeName: aws.String("pk"), AttributeType: types.ScalarAttributeTypeS},
			{AttributeName: aws.String("sk"), AttributeType: types.ScalarAttributeTypeN},
		},
		GlobalSecondaryIndexes: []types.GlobalSecondaryIndexDescription{{IndexName: aws.String("byStatus"), KeySchema: ks("status", "")}},
		LocalSecondaryIndexes:  []types.LocalSecondaryIndexDescription{{IndexName: aws.String("byDate"), KeySchema: ks("pk", "date")}},
	}
	var got []string
	for _, n := range tableNodes(d) {
		got = append(got, n.Name+"|"+n.Info)
	}
	want := []string{"항목 수|≈42", "PK pk|S", "SK sk|N", "gsi byStatus|status", "lsi byDate|pk/date"}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("nodes = %v, want %v", got, want)
	}
}

// ─── 페이지 이어 받기 ───

// fakeAPI 는 ExecuteStatement 마다 항목 perPage 개와 늘 NextToken 을 돌려준다(거르기로 거의 안 맞는 큰 테이블).
type fakeAPI struct {
	perPage  int
	calls    int
	deadline bool // 받은 ctx 에 기한이 있었는지
	block    func(ctx context.Context, call int) error
	rangeKey string // 있으면 테이블 정렬 키
}

func (f *fakeAPI) ListTables(context.Context, *dynamodb.ListTablesInput, ...func(*dynamodb.Options)) (*dynamodb.ListTablesOutput, error) {
	return &dynamodb.ListTablesOutput{}, nil
}

func (f *fakeAPI) DescribeTable(_ context.Context, in *dynamodb.DescribeTableInput, _ ...func(*dynamodb.Options)) (*dynamodb.DescribeTableOutput, error) {
	d := &types.TableDescription{TableName: in.TableName,
		KeySchema: []types.KeySchemaElement{{AttributeName: aws.String("pk"), KeyType: types.KeyTypeHash}}}
	if f.rangeKey != "" {
		d.KeySchema = append(d.KeySchema, types.KeySchemaElement{AttributeName: aws.String(f.rangeKey), KeyType: types.KeyTypeRange})
	}
	return &dynamodb.DescribeTableOutput{Table: d}, nil
}

func (f *fakeAPI) ExecuteStatement(ctx context.Context, _ *dynamodb.ExecuteStatementInput, _ ...func(*dynamodb.Options)) (*dynamodb.ExecuteStatementOutput, error) {
	f.calls++
	_, f.deadline = ctx.Deadline()
	if f.block != nil {
		if err := f.block(ctx, f.calls); err != nil {
			return nil, err
		}
	}
	out := &dynamodb.ExecuteStatementOutput{NextToken: aws.String("next")}
	for i := range f.perPage {
		out.Items = append(out.Items, map[string]types.AttributeValue{"pk": &types.AttributeValueMemberS{Value: fmt.Sprint(f.calls, "-", i)}})
	}
	return out, nil
}

func TestRun_페이지가_끝없이_이어지면_상한에서_멈추고_잘림으로_알린다(t *testing.T) {
	f := &fakeAPI{perPage: 0}
	b := &Backend{cl: f}
	res, err := b.Run(context.Background(), source.Query{Text: `SELECT * FROM "t" WHERE status = 'x'`, Force: true, Limit: 10})
	if err != nil {
		t.Fatal(err)
	}
	if f.calls != maxPages || !res.Truncated || !strings.Contains(res.Text, "페이지") {
		t.Fatalf("calls=%d truncated=%v text=%q, want %d번에서 멈추고 잘림", f.calls, res.Truncated, res.Text, maxPages)
	}
	if !f.deadline {
		t.Error("기한 없는 ctx 에도 기본 시간 상한을 걸어야 한다")
	}
}

func TestRun_기한이_지나면_받은_데까지_잘림으로_돌려준다(t *testing.T) {
	f := &fakeAPI{perPage: 1, block: func(ctx context.Context, call int) error {
		if call < 3 {
			return nil
		}
		<-ctx.Done()
		return ctx.Err()
	}}
	b := &Backend{cl: f}
	ctx, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
	defer cancel()
	res, err := b.Run(ctx, source.Query{Text: `SELECT * FROM "t"`, Force: true, Limit: 100})
	if err != nil {
		t.Fatal(err)
	}
	if len(res.Rows) != 2 || !res.Truncated || !strings.Contains(res.Text, "시간") {
		t.Fatalf("rows=%d truncated=%v text=%q, want 2행 잘림·시간 초과", len(res.Rows), res.Truncated, res.Text)
	}
}

func TestRun_받은_것_없이_기한이_지나면_오류다(t *testing.T) {
	f := &fakeAPI{block: func(ctx context.Context, _ int) error { <-ctx.Done(); return ctx.Err() }}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Millisecond)
	defer cancel()
	if _, err := (&Backend{cl: f}).Run(ctx, source.Query{Text: `SELECT * FROM "t"`, Force: true}); err == nil {
		t.Fatal("받은 항목이 없으면 오류여야 한다")
	}
}

// ─── R63.3 정렬 키 서버 정렬 ───

func TestSortKey_파티션_키_같음_조건이면_Run_이_읽은_정렬_키다(t *testing.T) {
	b := &Backend{cl: &fakeAPI{perPage: 1, rangeKey: "sk"}}
	q := `SELECT * FROM "t" WHERE pk = 'a'`
	if sk, reason := b.SortKey(q); sk != "" || reason != "키 정보 없음" {
		t.Fatalf("Run 전에는 키를 모른다: %q %q", sk, reason)
	}
	if _, err := b.Run(context.Background(), source.Query{Text: q, Limit: 1}); err != nil {
		t.Fatal(err)
	}
	cases := []struct{ q, sk, reason string }{
		{q, "sk", ""},
		{`SELECT * FROM "t" WHERE "pk" = 'a' AND sk > 3`, "sk", ""},
		{`SELECT * FROM "t" WHERE pk IN ['a','b']`, "", "파티션 키 = 조건 없음"},
		{`SELECT * FROM "t" WHERE status = 'x'`, "", "파티션 키 = 조건 없음"},
		{`SELECT * FROM "other" WHERE pk = 'a'`, "", "키 정보 없음"},
		{`UPDATE "t" SET a = 1`, "", "SELECT 가 아님"},
	}
	for _, c := range cases {
		if sk, reason := b.SortKey(c.q); sk != c.sk || reason != c.reason {
			t.Errorf("%s: %q %q", c.q, sk, reason)
		}
	}
	nosk := &Backend{cl: &fakeAPI{perPage: 1}}
	if _, err := nosk.Run(context.Background(), source.Query{Text: q, Limit: 1}); err != nil {
		t.Fatal(err)
	}
	if _, reason := nosk.SortKey(q); reason != "정렬 키 없는 테이블" {
		t.Errorf("정렬 키 없음: %q", reason)
	}
}

func TestSorted_맨_바깥_ORDER_BY_를_바꾸거나_붙인다(t *testing.T) {
	cases := []struct {
		text string
		desc bool
		want string
	}{
		{`SELECT * FROM "t" WHERE pk = 'a'`, false, `SELECT * FROM "t" WHERE pk = 'a' ORDER BY "sk" ASC`},
		{"SELECT * FROM \"t\" WHERE pk = 'a'; -- 메모", true, `SELECT * FROM "t" WHERE pk = 'a' ORDER BY "sk" DESC`},
		{"SELECT * FROM t WHERE pk = 'a' -- 메모\nORDER BY sk ASC", true, `SELECT * FROM t WHERE pk = 'a' ORDER BY "sk" DESC`},
		{`SELECT * FROM t WHERE pk = 'order' ORDER BY "sk" DESC;`, false, `SELECT * FROM t WHERE pk = 'order' ORDER BY "sk" ASC`},
	}
	for _, c := range cases {
		if got, ok := Sorted(c.text, "sk", c.desc); !ok || got != c.want {
			t.Errorf("%q → %v %q", c.text, ok, got)
		}
	}
	for _, text := range []string{`SELECT * FROM t WHERE pk = 'a' LIMIT 5`, `DELETE FROM t WHERE pk = 'a'`} {
		if got, ok := Sorted(text, "sk", false); ok {
			t.Errorf("%q → %q", text, got)
		}
	}
}
