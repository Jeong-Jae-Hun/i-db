// Package opensearch 는 OpenSearch 접속을 읽기 전용 source.Backend 로 다룬다(명세 R60·R61).
// 트리는 인덱스·별칭 → 매핑 필드이고, 편집기는 SQL 플러그인 SELECT 와 허용한 REST 읽기 경로만 보낸다.
package opensearch

import (
	"bytes"
	"cmp"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"maps"
	"math"
	"net/http"
	"net/url"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	v4 "github.com/aws/aws-sdk-go-v2/aws/signer/v4"
	"github.com/aws/aws-sdk-go-v2/config"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// maxWindow 는 OpenSearch 기본 index.max_result_window 다. 넘는 size·from+size 는 확인을 받는다(R60.2)
const maxWindow = 10000

type backend struct {
	base     *url.URL
	hc       *http.Client
	user     string
	password string
	// creds·region 은 sigv4 일 때만 채운다
	creds  aws.CredentialsProvider
	region string
	signer *v4.Signer

	// fields 는 정렬·거르기 다시 읽기(Requery)가 읽은 인덱스 매핑이다. 접속이 살아 있는 동안 둔다
	mu     sync.Mutex
	fields map[string][]field
}

// New 는 접속 하나를 만든다. c.Password 는 호출부가 .env 에서 고른 basic 비밀번호다.
// 네트워크는 첫 Children·Run 때 처음 쓴다(sigv4 자격 증명도 그때 받는다).
func New(c db.Conn) (source.Backend, error) {
	u, err := url.Parse(strings.TrimRight(c.URL, "/"))
	if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" {
		return nil, fmt.Errorf("opensearch: invalid url %q (need http(s)://host)", c.URL)
	}
	b := &backend{base: u, hc: source.HTTPClient()}
	switch c.Auth {
	case "":
	case "basic":
		if c.User == "" {
			return nil, errors.New("opensearch: basic auth needs user")
		}
		b.user, b.password = c.User, c.Password
	case "sigv4":
		opts := []func(*config.LoadOptions) error{}
		if c.Region != "" {
			opts = append(opts, config.WithRegion(c.Region))
		}
		if c.Profile != "" {
			opts = append(opts, config.WithSharedConfigProfile(c.Profile))
		}
		cfg, err := config.LoadDefaultConfig(context.Background(), opts...)
		if err != nil {
			return nil, fmt.Errorf("opensearch: load aws config: %w", err)
		}
		if cfg.Region == "" {
			return nil, errors.New("opensearch: sigv4 needs region (connection region or profile)")
		}
		b.creds, b.region, b.signer = cfg.Credentials, cfg.Region, v4.NewSigner()
	default:
		return nil, fmt.Errorf("opensearch: unsupported auth %q (basic, sigv4 or none)", c.Auth)
	}
	return b, nil
}

func (b *backend) Close() error { return nil }

// ─── 요청 ───

// do 는 path(질의 문자열 포함 가능)에 요청을 보내고 2xx 본문을 돌려준다.
// 오류는 OpenSearch 의 error.type·reason 을 싣고 비밀번호는 지운다.
func (b *backend) do(ctx context.Context, method, path string, body []byte) ([]byte, error) {
	u := *b.base
	p, rawQuery, _ := strings.Cut(strings.TrimLeft(path, "/"), "?")
	u.Path = b.base.Path + "/" + p
	u.RawPath = ""
	u.RawQuery = rawQuery
	req, err := http.NewRequestWithContext(ctx, method, u.String(), bytes.NewReader(body))
	if err != nil {
		return nil, fmt.Errorf("opensearch: %s", db.Redact(err.Error(), b.password))
	}
	req.Header.Set("Accept", "application/json")
	if len(body) > 0 {
		req.Header.Set("Content-Type", "application/json")
	}
	switch {
	case b.user != "":
		req.SetBasicAuth(b.user, b.password)
	case b.signer != nil:
		creds, err := b.creds.Retrieve(ctx)
		if err != nil {
			return nil, fmt.Errorf("opensearch: aws credentials: %w", err)
		}
		sum := sha256.Sum256(body)
		if err := b.signer.SignHTTP(ctx, creds, req, hex.EncodeToString(sum[:]), "es", b.region, time.Now()); err != nil {
			return nil, fmt.Errorf("opensearch: sigv4 sign: %w", err)
		}
	}
	resp, err := b.hc.Do(req)
	if err != nil {
		return nil, fmt.Errorf("opensearch: %s", db.Redact(err.Error(), b.password))
	}
	defer resp.Body.Close()
	if err := source.RedirectError(resp); err != nil {
		return nil, fmt.Errorf("opensearch: %w", err)
	}
	data, err := io.ReadAll(io.LimitReader(resp.Body, source.MaxBody+1))
	if err != nil {
		return nil, fmt.Errorf("opensearch: read response: %s", db.Redact(err.Error(), b.password))
	}
	if len(data) > source.MaxBody {
		return nil, fmt.Errorf("opensearch: response over %d MiB, narrow the query", source.MaxBody>>20)
	}
	if resp.StatusCode/100 != 2 {
		return nil, fmt.Errorf("opensearch %d: %s", resp.StatusCode, db.Redact(errorText(data), b.password))
	}
	return data, nil
}

// errorText 는 {"error":{"type","reason"}} 또는 {"error":"..."} 를 한 줄로 만든다. 모르는 꼴은 본문 앞부분이다.
func errorText(data []byte) string {
	var e struct {
		Error json.RawMessage `json:"error"`
	}
	if json.Unmarshal(data, &e) == nil && len(e.Error) > 0 {
		var obj struct {
			Type    string `json:"type"`
			Reason  string `json:"reason"`
			Details string `json:"details"`
		}
		if json.Unmarshal(e.Error, &obj) == nil && (obj.Type != "" || obj.Reason != "") {
			msg := strings.TrimPrefix(obj.Type+": "+obj.Reason, ": ")
			// SQL 플러그인 details 는 둘째 줄부터 안내 문구라 첫 줄만 싣는다
			if d, _, _ := strings.Cut(obj.Details, "\n"); d != "" && d != obj.Reason {
				msg += " (" + d + ")"
			}
			return msg
		}
		var s string
		if json.Unmarshal(e.Error, &s) == nil {
			return s
		}
	}
	const head = 300
	s := strings.TrimSpace(string(data))
	if len(s) > head {
		s = strings.ToValidUTF8(s[:head], "") + "…"
	}
	return s
}

// ─── 트리 ───

func (b *backend) Children(ctx context.Context, path string) ([]source.Node, error) {
	if path == "" {
		return b.indices(ctx)
	}
	if strings.Contains(path, "/") {
		return nil, nil // 필드는 잎이다
	}
	fields, err := b.mapping(ctx, path)
	if err != nil {
		return nil, err
	}
	nodes := make([]source.Node, 0, len(fields))
	for _, f := range fields {
		nodes = append(nodes, source.Node{Name: f.name, Path: path + "/" + f.name, Info: f.typ})
	}
	return nodes, nil
}

// indices 는 인덱스와 별칭(`별칭 → 인덱스`)을 이름순으로 돌려준다. `.` 으로 시작하는 시스템 인덱스·별칭은 숨긴다.
func (b *backend) indices(ctx context.Context) ([]source.Node, error) {
	data, err := b.do(ctx, http.MethodGet, "_cat/indices?format=json&h=index,docs.count,health", nil)
	if err != nil {
		return nil, err
	}
	var idx []struct {
		Index  string `json:"index"`
		Docs   string `json:"docs.count"`
		Health string `json:"health"`
	}
	if err := json.Unmarshal(data, &idx); err != nil {
		return nil, fmt.Errorf("opensearch: decode _cat/indices: %w", err)
	}
	data, err = b.do(ctx, http.MethodGet, "_cat/aliases?format=json&h=alias,index", nil)
	if err != nil {
		return nil, err
	}
	var aliases []struct {
		Alias string `json:"alias"`
		Index string `json:"index"`
	}
	if err := json.Unmarshal(data, &aliases); err != nil {
		return nil, fmt.Errorf("opensearch: decode _cat/aliases: %w", err)
	}
	var nodes []source.Node
	for _, i := range idx {
		if strings.HasPrefix(i.Index, ".") {
			continue
		}
		nodes = append(nodes, source.Node{Name: i.Index, Path: i.Index, Info: strings.TrimSpace(i.Docs + " " + i.Health), HasChildren: true, Openable: true})
	}
	// 별칭 하나가 인덱스 여럿을 가리키면 한 줄에 모은다
	targets := map[string][]string{}
	for _, a := range aliases {
		if strings.HasPrefix(a.Alias, ".") {
			continue
		}
		targets[a.Alias] = append(targets[a.Alias], a.Index)
	}
	for alias, ix := range targets {
		slices.Sort(ix)
		nodes = append(nodes, source.Node{Name: alias + " → " + strings.Join(ix, ", "), Path: alias, Info: "alias", HasChildren: true, Openable: true})
	}
	slices.SortFunc(nodes, func(a, b source.Node) int { return strings.Compare(a.Name, b.Name) })
	return nodes, nil
}

// field 는 매핑 필드 하나다. keyword 는 text 필드의 keyword 하위 필드 이름(`name.keyword` 꼴)이고 없으면 비었다.
type field struct{ name, typ, keyword string }

// mapping 은 인덱스(별칭·패턴이면 대상 전부)의 매핑 필드를 `a.b` 로 펴서 이름순으로 돌려준다.
// 같은 필드가 인덱스마다 타입이 다르면 이름순 첫 인덱스의 타입을 쓴다.
func (b *backend) mapping(ctx context.Context, index string) ([]field, error) {
	data, err := b.do(ctx, http.MethodGet, url.PathEscape(index)+"/_mapping", nil)
	if err != nil {
		return nil, err
	}
	var m map[string]struct {
		Mappings mappingProps `json:"mappings"`
	}
	if err := json.Unmarshal(data, &m); err != nil {
		return nil, fmt.Errorf("opensearch: decode _mapping: %w", err)
	}
	seen, kws := map[string]string{}, map[string]string{}
	for _, index := range slices.Sorted(maps.Keys(m)) {
		flatten(seen, "", m[index].Mappings.Properties)
		keywords(kws, "", m[index].Mappings.Properties)
	}
	fields := make([]field, 0, len(seen))
	for name, typ := range seen {
		fields = append(fields, field{name, typ, kws[name]})
	}
	slices.SortFunc(fields, func(a, b field) int { return strings.Compare(a.name, b.name) })
	return fields, nil
}

type mappingProps struct {
	Type       string                  `json:"type"`
	Properties map[string]mappingProps `json:"properties"`
	Fields     map[string]mappingProps `json:"fields"` // 다중 필드(text 의 keyword 등)
}

// flatten 은 properties 를 `a.b` 이름 → 타입으로 편다. 타입이 없는 object 는 줄을 만들지 않고 아래만 편다.
func flatten(out map[string]string, prefix string, props map[string]mappingProps) {
	for name, p := range props {
		full := prefix + name
		if _, ok := out[full]; !ok && p.Type != "" {
			out[full] = p.Type
		}
		flatten(out, full+".", p.Properties)
	}
}

// keywords 는 text 필드마다 keyword 타입 하위 필드 `이름.<하위>` 를 적는다. 여럿이면 이름이 "keyword" 인 것, 없으면 이름순 첫 것이다.
func keywords(out map[string]string, prefix string, props map[string]mappingProps) {
	for name, p := range props {
		full := prefix + name
		if _, ok := out[full]; !ok && p.Type == "text" {
			for _, sub := range slices.Sorted(maps.Keys(p.Fields)) {
				if p.Fields[sub].Type == "keyword" && (out[full] == "" || sub == "keyword") {
					out[full] = full + "." + sub
				}
			}
		}
		keywords(out, full+".", p.Properties)
	}
}

// ─── 실행 ───

func (b *backend) Open(ctx context.Context, path string, limit int) (*db.Result, error) {
	if limit <= 0 {
		limit = source.OpenLimit
	}
	index, _, _ := strings.Cut(path, "/")
	start := time.Now()
	body := fmt.Appendf(nil, openBody, limit)
	data, err := b.do(ctx, http.MethodPost, url.PathEscape(index)+"/_search", body)
	if err != nil {
		return nil, err
	}
	res, err := searchResult(data, limit)
	if err != nil {
		return nil, err
	}
	res.Elapsed = time.Since(start)
	return res, nil
}

// openBody 는 트리 Enter 미리 보기(Open)의 본문이다.
const openBody = `{"query":{"match_all":{}},"size":%d}`

// OpenText 는 Open(path, limit) 과 같은 요청을 편집기 꼴(`POST <인덱스>/_search` + 본문)로 쓴 글이다.
// 미리 보기 결과도 정렬을 서버로 다시 읽을 수 있게 결과의 원래 요청으로 남긴다(R61.3).
func OpenText(path string, limit int) string {
	if limit <= 0 {
		limit = source.OpenLimit
	}
	index, _, _ := strings.Cut(path, "/")
	return "POST " + index + "/_search\n" + fmt.Sprintf(openBody, limit)
}

// ─── 정렬·거르기 서버로 다시 읽기 (R61.3) ───

// Filter 는 _search 다시 읽기의 거르기 하나다(R14.4 꼴). Field 가 비면 모든 필드 찾기다.
// Op 는 "=" "!=" ">" ">=" "<" "<=" ":" "null" "notnull" 이고 모든 필드 찾기는 ":" 또는 "!:" 다.
type Filter struct {
	Field, Op, Value string
}

// Rewrite 는 _search 요청에 바꿔 넣을 정렬·거르기다. Sort 가 비면 본문 sort 를, Filter 가 nil 이면 본문 query 를 그대로 둔다.
type Rewrite struct {
	Sort   string
	Desc   bool
	Filter *Filter
}

// fieldInfo 는 필드의 매핑 타입과 keyword 하위 필드 이름이다. 모르면 둘 다 빈 글이다.
type fieldInfo func(field string) (typ, keyword string)

func noFieldInfo(string) (string, string) { return "", "" }

// IsSearch 는 text 가 허용한 `_search` 요청 글인지다. SELECT·_count·_cat·_mapping 은 아니다.
func IsSearch(text string) bool {
	method, path, _, ok := parseRequest(text)
	if !ok {
		return false
	}
	p, _, _ := strings.Cut(strings.TrimLeft(path, "/"), "?")
	endpoint, allowed := allowedPath(method, p)
	return allowed && endpoint == "_search"
}

// Requery 는 _search 요청 q.Text 의 sort·query 를 r 로 바꿔 b 로 보내고, 보낸 글을 같이 돌려준다(R61.3).
// b 가 이 패키지의 접속이면 인덱스 매핑을 읽어(인덱스마다 접속이 살아 있는 동안 한 번) keyword 하위 필드가 있는
// text 필드는 그 하위 필드로 정렬·비교한다. 매핑을 못 읽으면 필드 이름 그대로 보낸다.
func Requery(ctx context.Context, b source.Backend, q source.Query, r Rewrite) (string, *db.Result, error) {
	info := fieldInfo(noFieldInfo)
	if ob, ok := b.(*backend); ok {
		info = ob.fieldInfo(ctx, q.Text)
	}
	text, ok := rewrite(q.Text, r, info)
	if !ok {
		return "", nil, errors.New("opensearch: not a _search request with a JSON object body")
	}
	q.Text = text
	res, err := b.Run(ctx, q)
	return text, res, err
}

// fieldInfo 는 요청 경로의 인덱스 매핑에서 필드 정보를 찾는 함수다. 인덱스 없는 `_search` 는 모든 인덱스라 읽지 않는다.
func (b *backend) fieldInfo(ctx context.Context, text string) fieldInfo {
	_, path, _, _ := parseRequest(text)
	p, _, _ := strings.Cut(strings.TrimLeft(path, "/"), "?")
	index, _, ok := strings.Cut(p, "/")
	if !ok || index == "" {
		return noFieldInfo
	}
	b.mu.Lock()
	fields, cached := b.fields[index]
	b.mu.Unlock()
	if !cached {
		var err error
		if fields, err = b.mapping(ctx, index); err != nil {
			return noFieldInfo
		}
		b.mu.Lock()
		if b.fields == nil {
			b.fields = map[string][]field{}
		}
		b.fields[index] = fields
		b.mu.Unlock()
	}
	return func(name string) (string, string) {
		i, found := slices.BinarySearchFunc(fields, name, func(f field, n string) int { return strings.Compare(f.name, n) })
		if !found {
			return "", ""
		}
		return fields[i].typ, fields[i].keyword
	}
}

// rewrite 는 Requery 의 글 바꾸기다. 값이 없는 문서는 오름·내림 모두 뒤로 보내고, 거르기는 원래 query 를
// bool.must 에 두고 조건을 bool.filter(아니다 꼴은 must_not)에 더한다. _search 가 아니거나 본문이 JSON 객체가 아니면 false 다.
//   - 예: `GET logs/_search` + `{"size":20}`, Sort "status" Desc → `{"size":20,"sort":[{"status":{"missing":"_last","order":"desc"}}]}`
//   - 예: Filter status=500 → `{"query":{"bool":{"filter":[{"term":{"status":"500"}}],"must":[{"match_all":{}}]}}}`
func rewrite(text string, r Rewrite, info fieldInfo) (string, bool) {
	method, path, body, ok := parseRequest(text)
	if !ok || !IsSearch(text) {
		return "", false
	}
	var obj map[string]json.RawMessage
	if body != "" {
		if err := json.Unmarshal([]byte(body), &obj); err != nil {
			return "", false
		}
	}
	if obj == nil {
		obj = map[string]json.RawMessage{}
	}
	if r.Sort != "" {
		key := map[string]string{"order": "asc", "missing": "_last"}
		if r.Desc {
			key["order"] = "desc"
		}
		// _score 정렬은 missing 을 받지 않는다(모든 문서에 점수가 있다)
		if r.Sort == "_score" {
			delete(key, "missing")
		}
		sort, err := json.Marshal([]map[string]map[string]string{{exactField(r.Sort, info): key}})
		if err != nil {
			return "", false
		}
		obj["sort"] = sort
	}
	if r.Filter != nil {
		if clause, not := filterClause(*r.Filter, info); clause != nil {
			orig := obj["query"]
			if len(orig) == 0 {
				orig = json.RawMessage(`{"match_all":{}}`)
			}
			where := "filter"
			if not {
				where = "must_not"
			}
			q, err := json.Marshal(map[string]any{"bool": map[string]any{"must": []json.RawMessage{orig}, where: []any{clause}}})
			if err != nil {
				return "", false
			}
			obj["query"] = q
		}
	}
	out, err := json.Marshal(obj)
	if err != nil {
		return "", false
	}
	return method + " " + path + "\n" + string(out), true
}

// exactField 는 text 필드에 keyword 하위 필드가 있으면 그 이름이다. text 는 낱말로 쪼개 저장해 정렬·같음 비교가 안 된다.
func exactField(name string, info fieldInfo) string {
	if typ, kw := info(name); typ == "text" && kw != "" {
		return kw
	}
	return name
}

var wildcardEscaper = strings.NewReplacer(`\`, `\\`, `*`, `\*`, `?`, `\?`)

// filterClause 는 거르기를 query 절 하나로 바꾼다. not 이면 must_not 자리다. 모든 필드 찾기의 빈 값은 nil(거르기 없음)이다.
//   - 예: status=A → term, n>5 → range(십진수면 숫자), msg:err → wildcard *err*(대소문자 무시), msg=null → must_not exists.
//   - 예: 모든 필드 abc → query_string "abc"(따옴표로 감싸 연산자 글자를 막고 lenient 로 타입이 안 맞는 필드는 건너뜀).
func filterClause(f Filter, info fieldInfo) (any, bool) {
	if f.Field == "" {
		if f.Value == "" {
			return nil, false
		}
		phrase := `"` + strings.NewReplacer(`\`, `\\`, `"`, `\"`).Replace(f.Value) + `"`
		return map[string]any{"query_string": map[string]any{"query": phrase, "lenient": true}}, f.Op == "!:"
	}
	exists := map[string]any{"exists": map[string]string{"field": f.Field}}
	field := exactField(f.Field, info)
	switch f.Op {
	case "null":
		return exists, true
	case "notnull":
		return exists, false
	case "=", "!=":
		return map[string]any{"term": map[string]string{field: f.Value}}, f.Op == "!="
	case ":":
		if f.Value == "" {
			return exists, false
		}
		// wildcard 는 글자 필드에만 된다. 숫자·날짜 같은 필드는 같음으로 본다
		if typ, _ := info(f.Field); typ != "" && typ != "text" && typ != "keyword" && typ != "wildcard" {
			return map[string]any{"term": map[string]string{field: f.Value}}, false
		}
		return map[string]any{"wildcard": map[string]any{field: map[string]any{
			"value": "*" + wildcardEscaper.Replace(f.Value) + "*", "case_insensitive": true}}}, false
	}
	ops := map[string]string{">": "gt", ">=": "gte", "<": "lt", "<=": "lte"}
	var v any = f.Value
	if strings.Trim(f.Value, "0123456789+-.eE") == "" && json.Valid([]byte(f.Value)) {
		v = json.Number(f.Value)
	}
	return map[string]any{"range": map[string]any{field: map[string]any{ops[f.Op]: v}}}, false
}

// Run 은 SELECT 면 SQL 플러그인으로, `GET|POST <경로>` 첫 줄이면 허용한 REST 읽기 경로로 보낸다(R61.2).
// 그 밖은 보내지 않고 source.ErrWrite 다.
func (b *backend) Run(ctx context.Context, q source.Query) (*db.Result, error) {
	limit := q.Limit
	if limit <= 0 {
		limit = db.MaxRows
	}
	start := time.Now()
	var (
		res *db.Result
		err error
	)
	if db.FirstSQLWord(q.Text) == "select" {
		res, err = b.runSQL(ctx, q.Text, limit)
	} else {
		res, err = b.runREST(ctx, q, limit)
	}
	if err != nil {
		return nil, err
	}
	res.Elapsed = time.Since(start)
	return res, nil
}

func (b *backend) runSQL(ctx context.Context, text string, limit int) (*db.Result, error) {
	stmt, err := sqlStatement(text)
	if err != nil {
		return nil, err
	}
	// fetch_size 를 주면 플러그인 기본 상한(plugins.query.size_limit, 200행) 대신 첫 페이지로 limit 행까지 돌려준다.
	// 다음 페이지 커서는 따라가지 않는다(행 상한이 곧 첫 페이지 크기다)
	body, err := json.Marshal(map[string]any{"query": stmt, "fetch_size": min(limit, maxWindow)})
	if err != nil {
		return nil, err
	}
	data, err := b.do(ctx, http.MethodPost, "_plugins/_sql?format=jdbc", body)
	if err != nil {
		return nil, err
	}
	var r struct {
		Schema []struct {
			Name  string `json:"name"`
			Alias string `json:"alias"`
		} `json:"schema"`
		Datarows [][]json.RawMessage `json:"datarows"`
	}
	if err := json.Unmarshal(data, &r); err != nil {
		return nil, fmt.Errorf("opensearch: decode sql response: %w", err)
	}
	res := &db.Result{}
	for _, s := range r.Schema {
		res.Cols = append(res.Cols, cmp.Or(s.Alias, s.Name))
	}
	rows := r.Datarows
	if len(rows) > limit {
		rows, res.Truncated = rows[:limit], true
	}
	for _, dr := range rows {
		row := make([]string, len(res.Cols))
		for i := range row {
			row[i] = "NULL"
			if i < len(dr) {
				row[i] = cell(dr[i])
			}
		}
		res.Rows = append(res.Rows, row)
	}
	return res, nil
}

// sqlWriteWords 는 SQL 플러그인에 보내지 않을 쓰기 낱말이다. 문자열·주석 밖이면 어디 있든(하위 질의 포함) 거절한다.
// 백틱 이름(`delete`)도 걸리지만 안전 쪽이다.
var sqlWriteWords = []string{"delete", "update", "insert", "drop", "create", "alter", "truncate", "replace"}

// sqlStatement 는 SELECT 한 문장을 보낼 글로 다듬는다. 끝의 ;·공백·주석은 떼고, 그 뒤에도 문자열·주석 밖에 ; 가 남거나
// 쓰기 낱말이 있으면 source.ErrWrite 다(R61.2). 어휘는 db.MaskSQL(따옴표·주석 가리기)로 읽는다.
// 단, MaskSQL 은 PostgreSQL 의 $태그$ 도 문자열로 본다. OpenSearch SQL 에는 없는 꼴이라 그 안에 숨긴 글은 플러그인이 문법 오류로 거절한다.
func sqlStatement(text string) (string, error) {
	masked := db.MaskSQL(text)
	n := len(strings.TrimRight(masked, "; \t\r\n\f\v"))
	masked = masked[:n]
	if strings.Contains(masked, ";") {
		return "", fmt.Errorf("%w: multiple statements", source.ErrWrite)
	}
	words := strings.FieldsFunc(strings.ToLower(masked), func(r rune) bool { return r > 0x7f || !db.IsIdentByte(byte(r)) })
	for _, w := range words {
		if slices.Contains(sqlWriteWords, w) {
			return "", fmt.Errorf("%w: %s", source.ErrWrite, w)
		}
	}
	return strings.TrimSpace(text[:n]), nil
}

// searchEndpoints 는 `<인덱스>/<끝>` 꼴로 GET 할 수 있는 읽기 끝점이다. POST 는 _search 만 받는다.
var searchEndpoints = []string{"_search", "_count", "_mapping"}

func (b *backend) runREST(ctx context.Context, q source.Query, limit int) (*db.Result, error) {
	method, path, body, ok := parseRequest(q.Text)
	if !ok {
		return nil, fmt.Errorf("%w: expected SELECT … or GET <index>/_search", source.ErrWrite)
	}
	p, rawQuery, _ := strings.Cut(strings.TrimLeft(path, "/"), "?")
	params, err := url.ParseQuery(rawQuery)
	if err != nil {
		return nil, fmt.Errorf("opensearch: invalid query string: %w", err)
	}
	endpoint, allowed := allowedPath(method, p)
	if !allowed {
		return nil, fmt.Errorf("%w: %s /%s", source.ErrWrite, method, p)
	}
	if body != "" && !json.Valid([]byte(body)) {
		return nil, errors.New("opensearch: request body is not valid JSON")
	}
	if endpoint == "_search" && !q.Force {
		if err := checkWindow(body, params); err != nil {
			return nil, err
		}
	}
	if endpoint == "_cat" {
		params.Set("format", "json") // 표로 만들려면 JSON 이어야 한다
	}
	if len(params) > 0 {
		p += "?" + params.Encode()
	}
	data, err := b.do(ctx, method, p, []byte(body))
	if err != nil {
		return nil, err
	}
	switch endpoint {
	case "_search":
		return searchResult(data, limit)
	case "_count":
		var r struct {
			Count json.RawMessage `json:"count"`
		}
		if err := json.Unmarshal(data, &r); err != nil {
			return nil, fmt.Errorf("opensearch: decode _count: %w", err)
		}
		return &db.Result{Cols: []string{"count"}, Rows: [][]string{{cell(r.Count)}}}, nil
	case "_mapping":
		var m map[string]struct {
			Mappings mappingProps `json:"mappings"`
		}
		if err := json.Unmarshal(data, &m); err != nil {
			return nil, fmt.Errorf("opensearch: decode _mapping: %w", err)
		}
		res := &db.Result{Cols: []string{"index", "field", "type"}}
		for _, index := range slices.Sorted(maps.Keys(m)) {
			seen := map[string]string{}
			flatten(seen, "", m[index].Mappings.Properties)
			for _, name := range slices.Sorted(maps.Keys(seen)) {
				res.Rows = append(res.Rows, []string{index, name, seen[name]})
			}
		}
		if len(res.Rows) > limit {
			res.Rows, res.Truncated = res.Rows[:limit], true
		}
		return res, nil
	default: // _cat
		var items []json.RawMessage
		if err := json.Unmarshal(data, &items); err != nil {
			return nil, fmt.Errorf("opensearch: decode _cat: %w", err)
		}
		return objectTable(nil, items, limit)
	}
}

// parseRequest 는 빈 줄·주석 줄(`--`·`//`·`#`)을 건너뛴 첫 줄을 `메서드 경로` 로, 나머지를 JSON 본문으로 나눈다.
func parseRequest(text string) (method, path, body string, ok bool) {
	lines := strings.Split(text, "\n")
	for i, line := range lines {
		line = strings.TrimSpace(line)
		if line == "" || strings.HasPrefix(line, "--") || strings.HasPrefix(line, "//") || strings.HasPrefix(line, "#") {
			continue
		}
		f := strings.Fields(line)
		if len(f) != 2 {
			return "", "", "", false
		}
		return strings.ToUpper(f[0]), f[1], strings.TrimSpace(strings.Join(lines[i+1:], "\n")), true
	}
	return "", "", "", false
}

// allowedPath 는 읽기로 허용한 경로인지와 결과 꼴을 가를 끝점 이름을 돌려준다.
// GET: `_search`, `_cat/<이름>[/<인덱스>]`, `<인덱스>/_search|_count|_mapping`. POST: `_search`, `<인덱스>/_search`.
// 마디는 비거나 `.`·`..`·`%`·제어 문자를 가질 수 없다(서버·프록시가 경로를 다시 풀어 다른 API 로 가는 것을 막는다).
func allowedPath(method, p string) (string, bool) {
	segs := strings.Split(p, "/")
	if method != http.MethodGet && method != http.MethodPost || slices.ContainsFunc(segs, badSegment) {
		return "", false
	}
	switch {
	case len(segs) == 1 && segs[0] == "_search":
		return "_search", true
	case segs[0] == "_cat":
		ok := method == http.MethodGet && len(segs) <= 3 && len(segs) >= 2 && catName.MatchString(segs[1]) &&
			(len(segs) == 2 || indexOK(segs[2]))
		return "_cat", ok
	case len(segs) != 2 || !indexOK(segs[0]):
		return "", false
	case method == http.MethodPost:
		return "_search", segs[1] == "_search"
	}
	return segs[1], slices.Contains(searchEndpoints, segs[1])
}

var catName = regexp.MustCompile(`^[A-Za-z0-9_]+$`)

func badSegment(s string) bool {
	return s == "" || s == "." || s == ".." || strings.ContainsAny(s, "%\\") ||
		strings.ContainsFunc(s, func(r rune) bool { return r < 0x20 || r == 0x7f })
}

// indexOK 는 인덱스 자리(쉼표 목록)인지 본다. `_` 로 시작하면 안 되고(`_all` 만 받음) `_doc`·`_bulk` 같은 API 를
// 인덱스로 착각하지 않게 한다. `.` 으로 시작하는 시스템 인덱스(`.opendistro_security` 등)는 받지 않는다(R61.2).
func indexOK(s string) bool {
	for _, part := range strings.Split(s, ",") {
		part = strings.TrimPrefix(part, "-") // 빼기 꼴(-logs-old)도 같은 규칙
		if part == "" || strings.HasPrefix(part, ".") || strings.HasPrefix(part, "_") && part != "_all" {
			return false
		}
	}
	return true
}

// checkWindow 는 본문·질의 문자열의 size·from 이 maxWindow 를 넘으면 source.ErrNeedsConfirm 이다.
// 숫자는 문자열("20")·실수(2e4)도 OpenSearch 가 받으므로 같이 읽는다. 읽을 수 없거나 같은 키가 둘이면
// 서버가 어느 값을 쓸지 몰라 확인을 받는다(안전 쪽).
func checkWindow(body string, params url.Values) error {
	vals := map[string][]string{}
	if body != "" {
		keys, raws, err := orderedObject(json.RawMessage(body))
		if err != nil {
			return source.NeedsConfirm(unreadableWindow)
		}
		for i, k := range keys {
			if k == "size" || k == "from" {
				vals[k] = append(vals[k], jsonScalar(raws[i]))
			}
		}
	}
	window := map[string]float64{}
	for _, k := range []string{"size", "from"} {
		all := append(vals[k], params[k]...)
		if len(all) > 1 {
			return source.NeedsConfirm(unreadableWindow)
		}
		if len(all) == 0 {
			continue
		}
		v, err := strconv.ParseFloat(strings.TrimSpace(all[0]), 64)
		if err != nil || math.IsNaN(v) || math.IsInf(v, 0) {
			return source.NeedsConfirm(unreadableWindow)
		}
		window[k] = v
	}
	if window["size"] > maxWindow || window["from"]+window["size"] > maxWindow {
		return source.NeedsConfirm("size 1만 초과")
	}
	return nil
}

const unreadableWindow = "size·from 을 읽을 수 없음"

// jsonScalar 는 JSON 문자열이면 풀어 쓴 값, 아니면 원문이다(숫자·불·객체는 ParseFloat 가 가른다).
func jsonScalar(raw json.RawMessage) string {
	var s string
	if json.Unmarshal(raw, &s) == nil {
		return s
	}
	return string(raw)
}

// ─── 결과 ───

// searchResult 는 _search 응답을 `_index`·`_id`·`_score` + _source 최상위 필드 합집합(처음 본 순서) 표로 만든다.
// 집계가 있으면 `aggregations` 열과 그 JSON 을 담은 마지막 행을 더한다. 문서가 없으면 집계 열 하나짜리 표다.
func searchResult(data []byte, limit int) (*db.Result, error) {
	var r struct {
		Hits struct {
			Hits []struct {
				Index  json.RawMessage `json:"_index"`
				ID     json.RawMessage `json:"_id"`
				Score  json.RawMessage `json:"_score"`
				Source json.RawMessage `json:"_source"`
			} `json:"hits"`
		} `json:"hits"`
		Aggregations json.RawMessage `json:"aggregations"`
	}
	if err := json.Unmarshal(data, &r); err != nil {
		return nil, fmt.Errorf("opensearch: decode _search: %w", err)
	}
	hits := r.Hits.Hits
	if len(hits) == 0 && len(r.Aggregations) > 0 {
		return &db.Result{Cols: []string{"aggregations"}, Rows: [][]string{{cell(r.Aggregations)}}}, nil
	}
	truncated := false
	if len(hits) > limit {
		hits, truncated = hits[:limit], true
	}
	sources := make([]json.RawMessage, len(hits))
	for i, h := range hits {
		sources[i] = h.Source
	}
	res, err := objectTable([]string{"_index", "_id", "_score"}, sources, limit)
	if err != nil {
		return nil, err
	}
	res.Truncated = truncated
	for i, h := range hits {
		res.Rows[i][0], res.Rows[i][1], res.Rows[i][2] = cell(h.Index), cell(h.ID), cell(h.Score)
	}
	if len(r.Aggregations) > 0 {
		res.Cols = append(res.Cols, "aggregations")
		for i := range res.Rows {
			res.Rows[i] = append(res.Rows[i], "NULL")
		}
		last := make([]string, len(res.Cols))
		for i := range last {
			last[i] = "NULL"
		}
		last[len(last)-1] = cell(r.Aggregations)
		res.Rows = append(res.Rows, last)
	}
	return res, nil
}

// objectTable 은 JSON 객체 목록을 lead 열 + 최상위 키 합집합(처음 본 순서) 표로 만든다.
// lead 열 칸과 없는 키는 "NULL" 로 두고, limit 을 넘는 행은 버리고 Truncated 로 알린다.
func objectTable(lead []string, objs []json.RawMessage, limit int) (*db.Result, error) {
	res := &db.Result{Cols: slices.Clone(lead)}
	if len(objs) > limit {
		objs, res.Truncated = objs[:limit], true
	}
	pos := map[string]int{}
	for i, c := range lead {
		pos[c] = i // _source 에 같은 이름(_id 등)이 있어도 열을 겹쳐 만들지 않는다
	}
	decoded := make([]map[string]json.RawMessage, len(objs))
	for i, o := range objs {
		keys, vals, err := orderedObject(o)
		if err != nil {
			return nil, err
		}
		decoded[i] = make(map[string]json.RawMessage, len(keys))
		for k, key := range keys {
			if _, ok := pos[key]; !ok {
				pos[key] = len(res.Cols)
				res.Cols = append(res.Cols, key)
			}
			decoded[i][key] = vals[k]
		}
	}
	for _, m := range decoded {
		row := make([]string, len(res.Cols))
		for c, col := range res.Cols {
			row[c] = "NULL"
			if v, ok := m[col]; ok && c >= len(lead) {
				row[c] = cell(v)
			}
		}
		res.Rows = append(res.Rows, row)
	}
	return res, nil
}

// orderedObject 는 JSON 객체의 키를 글에 나온 순서대로 돌려준다(map 으로 풀면 순서를 잃는다). null·빈 값은 키 없음이다.
func orderedObject(raw json.RawMessage) ([]string, []json.RawMessage, error) {
	if len(raw) == 0 || string(raw) == "null" {
		return nil, nil, nil
	}
	dec := json.NewDecoder(bytes.NewReader(raw))
	if t, err := dec.Token(); err != nil || t != json.Delim('{') {
		return nil, nil, fmt.Errorf("opensearch: expected JSON object, got %.40s", raw)
	}
	var (
		keys []string
		vals []json.RawMessage
	)
	for dec.More() {
		t, err := dec.Token()
		if err != nil {
			return nil, nil, fmt.Errorf("opensearch: decode object: %w", err)
		}
		var v json.RawMessage
		if err := dec.Decode(&v); err != nil {
			return nil, nil, fmt.Errorf("opensearch: decode object: %w", err)
		}
		keys, vals = append(keys, t.(string)), append(vals, v)
	}
	return keys, vals, nil
}

// cell 은 JSON 값을 표 칸 글로 바꾼다. null·없음은 "NULL"(db.Result 관례), 문자열은 따옴표 없이,
// 숫자·불은 원문, 객체·배열은 압축 JSON(값 보기 R32 로 연다)이다. MaxCellBytes 를 넘으면 자른다.
func cell(raw json.RawMessage) string {
	raw = bytes.TrimSpace(raw)
	if len(raw) == 0 || string(raw) == "null" {
		return "NULL"
	}
	var s string
	switch raw[0] {
	case '"':
		if json.Unmarshal(raw, &s) != nil {
			s = string(raw)
		}
	case '{', '[':
		var buf bytes.Buffer
		if json.Compact(&buf, raw) != nil {
			s = string(raw)
		} else {
			s = buf.String()
		}
	default:
		s = string(raw)
	}
	return db.CapCell(s)
}
