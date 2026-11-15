// Package obs 는 Loki·Prometheus·Grafana 를 source.Backend 로 다룬다(명세 R60·R62).
// 질의 API(labels·values·query·query_range)와 Grafana 데이터 소스 목록만 GET 으로 부르므로
// 쓰기 요청은 만들 수 없다. 그래서 ErrWrite 로 거를 문장도 없다.
package obs

import (
	"cmp"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

const maxErrBytes = 512

// client 는 저장소 하나로 가는 HTTP GET 이다. base 는 끝 '/' 없는 주소이고,
// Grafana 프록시 아래 Loki·Prometheus 는 같은 hc·인증에 base 만 다른 client 를 쓴다.
type client struct {
	kind   string // 오류 앞머리(loki·prometheus·grafana)
	base   string
	hc     *http.Client
	auth   func(*client, *http.Request)
	user   string // basic 사용자
	secret string // basic 비밀번호·토큰. 오류에서 지운다
}

// newClient 는 접속 주소·인증을 확인한다. 주소에 user:pass 가 붙어 있으면 떼어 basic 인증으로 쓴다
// (오류 문구에 주소가 찍혀도 비밀번호가 나오지 않게).
func newClient(kind string, c db.Conn, bearerOnly bool) (*client, error) {
	u, err := url.Parse(strings.TrimSpace(c.URL))
	if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" {
		return nil, fmt.Errorf("%s: url must be http(s)://host[:port][/path]", kind)
	}
	user, pass := c.User, c.Password
	if u.User != nil && c.Auth == "" && !bearerOnly {
		// 인증 없음인데 주소에 user:pass 가 있으면 조용히 버리지 않고 알린다(무엇을 보낼지 모호하다)
		return nil, fmt.Errorf("%s: url has user info but auth is none (set auth basic)", kind)
	}
	if u.User != nil {
		if user == "" {
			user = u.User.Username()
		}
		if p, ok := u.User.Password(); ok && pass == "" {
			pass = p
		}
		u.User = nil
	}
	cl := &client{kind: kind, base: strings.TrimRight(u.String(), "/"), hc: source.HTTPClient(), user: user, secret: pass}
	switch {
	case bearerOnly || c.Auth == "token":
		cl.auth = authBearer
	case c.Auth == "basic":
		cl.auth = authBasic
	case c.Auth == "":
		cl.auth = authNone
	default:
		return nil, fmt.Errorf("%s: unsupported auth %q (basic, token or none)", kind, c.Auth)
	}
	return cl, nil
}

func authBearer(c *client, r *http.Request) {
	if c.secret != "" {
		r.Header.Set("Authorization", "Bearer "+c.secret)
	}
}

func authBasic(c *client, r *http.Request) { r.SetBasicAuth(c.user, c.secret) }

func authNone(*client, *http.Request) {}

// sub 는 같은 연결·인증으로 base 아래 경로에 붙는 client 다(Grafana 데이터 소스 프록시).
func (c *client) sub(kind, prefix string) *client {
	cp := *c
	cp.kind, cp.base = kind, c.base+prefix
	return &cp
}

// get 은 base+path 를 GET 하고 2xx 본문을 돌려준다. 오류에는 서버 메시지를 담되 비밀값은 지운다.
func (c *client) get(ctx context.Context, path string, params url.Values) ([]byte, error) {
	target := c.base + path
	if len(params) > 0 {
		target += "?" + params.Encode()
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, target, nil)
	if err != nil {
		return nil, c.redact(err)
	}
	req.Header.Set("Accept", "application/json")
	c.auth(c, req)
	resp, err := c.hc.Do(req)
	if err != nil {
		return nil, c.redact(err)
	}
	defer resp.Body.Close()
	if err := source.RedirectError(resp); err != nil {
		return nil, fmt.Errorf("%s: %w", c.kind, err)
	}
	body, err := io.ReadAll(io.LimitReader(resp.Body, source.MaxBody+1))
	if err != nil {
		return nil, c.redact(err)
	}
	if resp.StatusCode/100 != 2 {
		return nil, c.redact(fmt.Errorf("%s %s: %s", c.kind, resp.Status, serverMessage(body)))
	}
	if len(body) > source.MaxBody {
		return nil, fmt.Errorf("%s: response larger than %d MiB, narrow the query", c.kind, source.MaxBody>>20)
	}
	return body, nil
}

// getData 는 Loki·Prometheus 봉투({"status":"success","data":…})에서 data 를 꺼낸다.
func (c *client) getData(ctx context.Context, path string, params url.Values, out any) error {
	body, err := c.get(ctx, path, params)
	if err != nil {
		return err
	}
	var env struct {
		Status string          `json:"status"`
		Data   json.RawMessage `json:"data"`
		Error  string          `json:"error"`
	}
	if err := json.Unmarshal(body, &env); err != nil {
		return fmt.Errorf("%s: unexpected response: %w", c.kind, err)
	}
	if env.Status != "success" {
		return c.redact(fmt.Errorf("%s: %s", c.kind, cmp.Or(env.Error, env.Status)))
	}
	if err := json.Unmarshal(env.Data, out); err != nil {
		return fmt.Errorf("%s: unexpected data: %w", c.kind, err)
	}
	return nil
}

func (c *client) close() { c.hc.CloseIdleConnections() }

func (c *client) redact(err error) error {
	return errors.New(db.Redact(err.Error(), c.secret))
}

// serverMessage 는 Prometheus({"error"})·Grafana({"message"}) JSON 이면 그 메시지를, Loki 처럼 평문이면 본문을 줄여 쓴다.
func serverMessage(body []byte) string {
	var m struct{ Error, Message string }
	if json.Unmarshal(body, &m) == nil && cmp.Or(m.Error, m.Message) != "" {
		return cmp.Or(m.Error, m.Message)
	}
	s := strings.TrimSpace(string(body))
	if len(s) > maxErrBytes {
		s = s[:maxErrBytes] + "…"
	}
	return s
}

// window 는 Query 의 Since·Until 을 [start, end] 로 바꾼다. 7일을 넘으면 Force 없이는 ErrNeedsConfirm 이다.
func window(q source.Query, now time.Time) (time.Time, time.Time, error) {
	since := q.Since
	if since <= 0 {
		since = source.DefaultRange
	}
	if q.Until < 0 || q.Until >= since {
		return time.Time{}, time.Time{}, fmt.Errorf("empty range: until %s must be shorter than since %s", q.Until, since)
	}
	if since-q.Until > source.ConfirmRange && !q.Force {
		return time.Time{}, time.Time{}, source.NeedsConfirm("범위 7일 초과")
	}
	return now.Add(-since), now.Add(-q.Until), nil
}

// step 은 범위 질의 해상도다. 한 시계열 250점 안팎, 최소 15초(Prometheus 11,000점 상한에서 멀다).
func step(start, end time.Time) string {
	s := max(end.Sub(start)/250, 15*time.Second)
	return fmt.Sprintf("%d", int64(s/time.Second))
}

func limitOf(n, def, ceiling int) int {
	if n <= 0 {
		n = def
	}
	if ceiling > 0 {
		n = min(n, ceiling)
	}
	return n
}

var errEmptyQuery = errors.New("empty query")
