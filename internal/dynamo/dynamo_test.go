package dynamo

import (
	"context"
	"errors"
	"reflect"
	"strings"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
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
	got := capText(big)
	if !db.IsCappedCell(got) {
		t.Errorf("잘린 칸을 db.IsCappedCell 이 알아봐야 한다: 꼬리 %q", got[len(got)-30:])
	}
	if capText("짧음") != "짧음" {
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
