package dynamo

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strconv"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb/types"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// newLocal 은 DBTUI_TEST_DYNAMO_URL(DynamoDB Local)에 붙고, pk·sk 와 GSI 가 있는 테이블에 항목 n 개를 넣는다.
// 테이블은 테스트가 끝나면 지운다.
func newLocal(t *testing.T, n int) (*Backend, string) {
	t.Helper()
	url := os.Getenv("DBTUI_TEST_DYNAMO_URL")
	if url == "" {
		t.Skip("DBTUI_TEST_DYNAMO_URL not set")
	}
	t.Setenv("AWS_ACCESS_KEY_ID", "x")
	t.Setenv("AWS_SECRET_ACCESS_KEY", "x")
	t.Setenv("AWS_SESSION_TOKEN", "")
	t.Setenv("AWS_PROFILE", "")
	t.Setenv("AWS_CONFIG_FILE", filepath.Join(t.TempDir(), "none"))
	t.Setenv("AWS_SHARED_CREDENTIALS_FILE", filepath.Join(t.TempDir(), "none"))
	t.Setenv("AWS_EC2_METADATA_DISABLED", "true")
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	t.Cleanup(cancel)
	sb, err := New(ctx, db.Conn{Kind: db.KindDynamoDB, URL: url, Region: "ap-northeast-2"})
	if err != nil {
		t.Fatal(err)
	}
	b := sb.(*Backend)
	table := fmt.Sprintf("ddbtest-%d", time.Now().UnixNano())
	_, err = b.cl.CreateTable(ctx, &dynamodb.CreateTableInput{
		TableName:   aws.String(table),
		BillingMode: types.BillingModePayPerRequest,
		AttributeDefinitions: []types.AttributeDefinition{
			{AttributeName: aws.String("pk"), AttributeType: types.ScalarAttributeTypeS},
			{AttributeName: aws.String("sk"), AttributeType: types.ScalarAttributeTypeN},
			{AttributeName: aws.String("status"), AttributeType: types.ScalarAttributeTypeS},
		},
		KeySchema: []types.KeySchemaElement{
			{AttributeName: aws.String("pk"), KeyType: types.KeyTypeHash},
			{AttributeName: aws.String("sk"), KeyType: types.KeyTypeRange},
		},
		GlobalSecondaryIndexes: []types.GlobalSecondaryIndex{{
			IndexName:  aws.String("byStatus"),
			KeySchema:  []types.KeySchemaElement{{AttributeName: aws.String("status"), KeyType: types.KeyTypeHash}},
			Projection: &types.Projection{ProjectionType: types.ProjectionTypeAll},
		}},
	})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_, _ = b.cl.DeleteTable(context.Background(), &dynamodb.DeleteTableInput{TableName: aws.String(table)})
	})
	for i := range n {
		pk := "u#1"
		if i%2 == 1 {
			pk = "u#2"
		}
		item := map[string]types.AttributeValue{
			"pk":     &types.AttributeValueMemberS{Value: pk},
			"sk":     &types.AttributeValueMemberN{Value: strconv.Itoa(i)},
			"status": &types.AttributeValueMemberS{Value: "open"},
			"meta":   &types.AttributeValueMemberM{Value: map[string]types.AttributeValue{"n": &types.AttributeValueMemberN{Value: "1.0"}}},
		}
		if _, err := b.cl.PutItem(ctx, &dynamodb.PutItemInput{TableName: aws.String(table), Item: item}); err != nil {
			t.Fatal(err)
		}
	}
	return b, table
}

func TestDynamoLocal_트리는_테이블_키_GSI를_보인다(t *testing.T) {
	b, table := newLocal(t, 3)
	ctx := context.Background()
	roots, err := b.Children(ctx, "")
	if err != nil {
		t.Fatal(err)
	}
	found := false
	for _, n := range roots {
		if n.Name == table {
			found = n.HasChildren && n.Openable && n.Path == table
		}
	}
	if !found {
		t.Fatalf("맨 위에 %s 가 펼칠 수 있는 줄로 있어야 한다: %+v", table, roots)
	}
	kids, err := b.Children(ctx, table)
	if err != nil {
		t.Fatal(err)
	}
	var got []string
	for _, n := range kids[1:] { // 항목 수는 DynamoDB Local 이 바로 갱신하지 않아 값은 보지 않는다
		got = append(got, n.Name+"|"+n.Info)
	}
	if want := []string{"PK pk|S", "SK sk|N", "gsi byStatus|status"}; kids[0].Name != "항목 수" || !reflect.DeepEqual(got, want) {
		t.Errorf("children = %+v, want 항목 수 + %v", kids, want)
	}
}

func TestDynamoLocal_파티션_키_조건이_없으면_확인받고_Force면_스캔한다(t *testing.T) {
	b, table := newLocal(t, 6)
	ctx := context.Background()

	res, err := b.Run(ctx, source.Query{Text: fmt.Sprintf(`SELECT * FROM "%s" WHERE "pk" = 'u#1'`, table)})
	if err != nil {
		t.Fatal(err)
	}
	if want := []string{"pk", "sk", "meta", "status"}; !reflect.DeepEqual(res.Cols, want) || len(res.Rows) != 3 {
		t.Fatalf("Cols=%v rows=%d, want %v 3행", res.Cols, len(res.Rows), want)
	}
	if res.Rows[0][2] != `{"n":1.0}` || res.Types[1] != "N" {
		t.Errorf("M 은 JSON 칸, 숫자는 원문이어야 한다: %v types=%v", res.Rows[0], res.Types)
	}

	scan := source.Query{Text: fmt.Sprintf(`SELECT * FROM "%s"`, table)}
	if _, err := b.Run(ctx, scan); !errors.Is(err, source.ErrNeedsConfirm) {
		t.Fatalf("전체 스캔 err = %v, want ErrNeedsConfirm", err)
	}
	scan.Force = true
	if res, err = b.Run(ctx, scan); err != nil || len(res.Rows) != 6 || res.Truncated {
		t.Fatalf("Force 스캔 = %v rows, truncated=%v, err=%v, want 6행", len(res.Rows), res.Truncated, err)
	}

	gsi := source.Query{Text: fmt.Sprintf(`SELECT * FROM "%s"."byStatus" WHERE "status" = 'open'`, table)}
	if res, err = b.Run(ctx, gsi); err != nil || len(res.Rows) != 6 {
		t.Fatalf("GSI 파티션 키 조건 = %v, err=%v, want 6행 확인 없이", res, err)
	}
	gsi.Text = fmt.Sprintf(`SELECT * FROM "%s"."byStatus" WHERE "pk" = 'u#1'`, table)
	if _, err = b.Run(ctx, gsi); !errors.Is(err, source.ErrNeedsConfirm) {
		t.Errorf("인덱스 키가 아닌 조건 err = %v, want ErrNeedsConfirm", err)
	}

	if _, err = b.Run(ctx, source.Query{Text: fmt.Sprintf(`DELETE FROM "%s" WHERE "pk" = 'u#1' AND "sk" = 0`, table), Force: true}); !errors.Is(err, source.ErrWrite) {
		t.Errorf("DELETE err = %v, want ErrWrite", err)
	}
}

func TestDynamoLocal_작은_한도면_페이지를_이어_자르고_잘림을_표시한다(t *testing.T) {
	b, table := newLocal(t, 7)
	ctx := context.Background()
	q := source.Query{Text: fmt.Sprintf(`SELECT * FROM "%s"`, table), Force: true, Limit: 3}
	res, err := b.Run(ctx, q)
	if err != nil || len(res.Rows) != 3 || !res.Truncated {
		t.Fatalf("Limit 3 = %v, err=%v, want 3행 잘림", res, err)
	}
	q.Limit = 7
	if res, err = b.Run(ctx, q); err != nil || len(res.Rows) != 7 || res.Truncated {
		t.Fatalf("Limit 7 = %d행 truncated=%v err=%v, want 7행 안 잘림", len(res.Rows), res.Truncated, err)
	}

	// 거르기가 있으면 Limit 은 읽을 항목 수라 한 페이지에 덜 오고 NextToken 이 남는다. 이어 받아야 다 모인다
	q = source.Query{Text: fmt.Sprintf(`SELECT * FROM "%s" WHERE "sk" >= 4`, table), Force: true, Limit: 3}
	if res, err = b.Run(ctx, q); err != nil || len(res.Rows) != 3 || res.Truncated {
		t.Fatalf("거르기 Limit 3 = %d행 truncated=%v err=%v, want 3행(sk 4·5·6) 안 잘림", len(res.Rows), res.Truncated, err)
	}
	q.Limit = 2
	if res, err = b.Run(ctx, q); err != nil || len(res.Rows) != 2 || !res.Truncated {
		t.Fatalf("거르기 Limit 2 = %d행 truncated=%v err=%v, want 2행 잘림", len(res.Rows), res.Truncated, err)
	}

	if res, err = b.OpenForce(ctx, table, 4); err != nil || len(res.Rows) != 4 || !res.Truncated || res.Cols[0] != "pk" {
		t.Fatalf("OpenForce 4 = %v, err=%v, want pk 먼저 4행 잘림", res, err)
	}
	if res, err = b.OpenForce(ctx, table, 0); err != nil || len(res.Rows) != 7 || res.Truncated {
		t.Fatalf("OpenForce 기본 50 = %v, err=%v, want 7행", res, err)
	}
}

func TestDynamoLocal_없는_테이블_오류는_코드와_메시지만_싣는다(t *testing.T) {
	b, _ := newLocal(t, 0)
	_, err := b.Run(context.Background(), source.Query{Text: `SELECT * FROM "no-such-table" WHERE pk = 'a'`})
	if err == nil {
		t.Fatal("없는 테이블은 오류여야 한다")
	}
	if want := "dynamodb DescribeTable: ResourceNotFoundException: "; len(err.Error()) < len(want) || err.Error()[:len(want)] != want {
		t.Errorf("err = %q, want 접두 %q", err, want)
	}
}
