package opensearch

import (
	"bytes"
	"context"
	"fmt"
	"net/http"
	"os"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// 실제 OpenSearch(보안 끔)에 인덱스를 만들어 트리·미리 보기·SQL·_search 를 확인한다.
// DBTUI_TEST_OPENSEARCH_URL 이 없으면 건너뛴다.
func TestIntegration_실제_OpenSearch_에서_트리와_질의를_읽는다(t *testing.T) {
	base := os.Getenv("DBTUI_TEST_OPENSEARCH_URL")
	if base == "" {
		t.Skip("DBTUI_TEST_OPENSEARCH_URL not set")
	}
	ctx, cancel := context.WithTimeout(context.Background(), time.Minute)
	defer cancel()
	index := fmt.Sprintf("ostest-%d", time.Now().UnixNano())
	sendRaw(t, ctx, http.MethodPut, base+"/"+index, `{"mappings":{"properties":{"msg":{"type":"keyword"},"n":{"type":"integer"},"user":{"properties":{"id":{"type":"long"}}}}}}`)
	t.Cleanup(func() {
		req, _ := http.NewRequest(http.MethodDelete, base+"/"+index, nil)
		if resp, err := http.DefaultClient.Do(req); err == nil {
			resp.Body.Close()
		}
	})
	sendRaw(t, ctx, http.MethodPost, base+"/_bulk?refresh=true", fmt.Sprintf(`{"index":{"_index":%q,"_id":"1"}}
{"msg":"a","n":1,"user":{"id":7}}
{"index":{"_index":%q,"_id":"2"}}
{"msg":"b","n":2,"tags":["x"]}
{"index":{"_index":%q,"_id":"3"}}
{"msg":"b","n":3}
`, index, index, index))

	b, err := New(db.Conn{URL: base})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()

	nodes, err := b.Children(ctx, "")
	if err != nil {
		t.Fatal(err)
	}
	if !slices.ContainsFunc(nodes, func(n source.Node) bool { return n.Path == index && strings.HasPrefix(n.Info, "3 ") }) {
		t.Fatalf("인덱스 %s 가 트리에 없다: %v", index, nodes)
	}
	fields, err := b.Children(ctx, index)
	if err != nil {
		t.Fatal(err)
	}
	var names []string
	for _, f := range fields {
		names = append(names, f.Name+" "+f.Info)
	}
	if !slices.Equal(names, []string{"msg keyword", "n integer", "tags text", "user.id long"}) {
		t.Fatalf("fields = %v", names)
	}

	res, err := b.Open(ctx, index, 0)
	if err != nil {
		t.Fatal(err)
	}
	if len(res.Rows) != 3 || !slices.Contains(res.Cols, "tags") || !slices.Contains(res.Cols, "user") {
		t.Fatalf("open = %v %d행", res.Cols, len(res.Rows))
	}

	res, err = b.Run(ctx, source.Query{Text: fmt.Sprintf("SELECT msg, COUNT(*) AS c FROM %s GROUP BY msg ORDER BY msg", index)})
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(res.Cols, []string{"msg", "c"}) || len(res.Rows) != 2 || !slices.Equal(res.Rows[1], []string{"b", "2"}) {
		t.Fatalf("sql = %v %v", res.Cols, res.Rows)
	}

	res, err = b.Run(ctx, source.Query{Text: fmt.Sprintf("GET %s/_search\n{\"query\":{\"term\":{\"msg\":\"b\"}},\"size\":1,\"aggs\":{\"total\":{\"sum\":{\"field\":\"n\"}}}}", index)})
	if err != nil {
		t.Fatal(err)
	}
	if len(res.Rows) != 2 || res.Cols[len(res.Cols)-1] != "aggregations" || res.Rows[1][len(res.Cols)-1] != `{"total":{"value":5.0}}` {
		t.Fatalf("search = %v %v", res.Cols, res.Rows)
	}

	res, err = b.Run(ctx, source.Query{Text: "GET " + index + "/_count"})
	if err != nil || res.Rows[0][0] != "3" {
		t.Fatalf("count = %v %v", res, err)
	}

	if _, err := b.Run(ctx, source.Query{Text: "GET nope-" + index + "/_search"}); err == nil {
		t.Fatal("없는 인덱스는 오류여야 한다")
	}
}

// sendRaw 는 시험 데이터를 만드는 요청이다. Backend 는 읽기 전용이라 직접 보낸다.
func sendRaw(t *testing.T, ctx context.Context, method, url, body string) {
	t.Helper()
	req, err := http.NewRequestWithContext(ctx, method, url, bytes.NewBufferString(body))
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Content-Type", "application/x-ndjson")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode/100 != 2 {
		t.Fatalf("%s %s: %d", method, url, resp.StatusCode)
	}
}
