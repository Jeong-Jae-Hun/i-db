package db

import (
	"bytes"
	"cmp"
	"context"
	"crypto/rand"
	"encoding/json"
	"errors"
	"fmt"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"

	"github.com/Jeong-Jae-Hun/i-db/internal/atomicfile"
)

// ─── db 전용 접속 (R59) ───
// DataGrip .idea 는 고치지 않고 .dbtui/connections.json 에 접속을, .env 에 비밀번호를 둔다.
// 파일 잠금은 하지 않는다. 화면과 CLI 가 같은 프로젝트를 동시에 고치면 읽고-고치고-쓰는 사이에 나중 쓰기가 앞 쓰기를 덮을 수 있다.
// 한 사람이 쓰는 개발 도구라 받아들인 한계이고, 필요해지면 .dbtui/.lock 에 flock 을 잡아 읽기부터 쓰기까지 감싼다.

const (
	DefaultPGPort    = 5432
	DefaultRedisPort = 6379
)

var connNameRe = regexp.MustCompile(`^[A-Za-z0-9_-]+$`)

// ValidConnName 은 db 전용 접속 이름 규칙(영숫자·-·_, R59.2)에 맞는지다.
func ValidConnName(name string) bool { return connNameRe.MatchString(name) }

// DefaultPort 는 종류의 기본 포트다. 읽기 전용 저장소는 포트를 URL 에 두어 0 이다.
func DefaultPort(k Kind) int {
	if k.IsSource() {
		return 0
	}
	if k == KindRedis {
		return DefaultRedisPort
	}
	return DefaultPGPort
}

// connEntry 는 connections.json 의 한 줄이다. 비밀번호는 넣지 않는다(.env 에 둔다).
type connEntry struct {
	UUID  string `json:"uuid"`
	Name  string `json:"name"`
	Group string `json:"group"`
	Kind  Kind   `json:"kind"`
	Host  string `json:"host"`
	Port  int    `json:"port"`
	DB    string `json:"db"`
	User  string `json:"user"`
	// R60.1 읽기 전용 저장소 칸. PostgreSQL·Redis 줄에는 쓰지 않는다
	URL     string `json:"url,omitempty"`
	Auth    string `json:"auth,omitempty"`
	Region  string `json:"region,omitempty"`
	Profile string `json:"profile,omitempty"`
	SSLMode string `json:"sslmode,omitempty"`
	// R68 Athena
	Workgroup string `json:"workgroup,omitempty"`
	Output    string `json:"output,omitempty"`
}

func connsPath(dir string) string { return filepath.Join(dir, ".dbtui", "connections.json") }

func readConnEntries(dir string) ([]connEntry, error) {
	b, err := os.ReadFile(connsPath(dir))
	if os.IsNotExist(err) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", connsPath(dir), err)
	}
	var es []connEntry
	if err := json.Unmarshal(b, &es); err != nil {
		return nil, fmt.Errorf("read %s: %w", connsPath(dir), err)
	}
	return es, nil
}

func writeConnEntries(dir string, es []connEntry) error {
	if es == nil {
		es = []connEntry{} // 다 지워도 null 이 아니라 빈 배열로 남긴다
	}
	b, err := json.MarshalIndent(es, "", "  ")
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(connsPath(dir)), 0o755); err != nil {
		return err
	}
	path, perm, err := writeTarget(connsPath(dir), 0o644)
	if err != nil {
		return err
	}
	return atomicfile.Write(path, ".connections-*.json", append(b, '\n'), perm)
}

// writeTarget 은 path 를 바꿔 끼워 쓸 실제 경로와 권한이다. 심볼릭 링크면 링크를 일반 파일로 바꾸지 않게 가리키는 파일에 쓴다
// (.env 를 다른 곳에 두고 링크로 쓰는 경우). 실제 경로가 폴더거나 가리키는 파일이 없는 링크면 거절한다.
// 파일이 없으면 path 에 newPerm 으로 만들고, 있으면 권한을 그대로 둔다.
func writeTarget(path string, newPerm os.FileMode) (string, os.FileMode, error) {
	real, err := filepath.EvalSymlinks(path)
	if os.IsNotExist(err) {
		if st, lerr := os.Lstat(path); lerr == nil && st.Mode()&os.ModeSymlink != 0 {
			return "", 0, fmt.Errorf("%s is a symlink to a missing file", path)
		}
		return path, newPerm, nil
	}
	if err != nil {
		return "", 0, err
	}
	st, err := os.Stat(real)
	if err != nil {
		return "", 0, err
	}
	if st.IsDir() {
		return "", 0, fmt.Errorf("%s is a directory", real)
	}
	return real, st.Mode().Perm(), nil
}

// loadDBConns 는 .idea 접속 뒤에 db 전용 접속을 붙인다. 이름·uuid 가 .idea 접속과 겹치면 빼고 경고한다(R59.1).
func (p *Project) loadDBConns(env map[string]string) error {
	es, err := readConnEntries(p.Dir)
	if err != nil {
		return err
	}
	for _, e := range es {
		c := e.conn()
		// uuid 를 읽을 때마다 만들면 다시 읽기(R59)·매핑·세션이 같은 접속을 찾지 못해, 손으로 고쳐 uuid 를 뺀 줄은 건너뛴다
		if c.UUID == "" {
			p.Warnings = append(p.Warnings, fmt.Sprintf("%s: missing uuid (connections.json)", c.Name))
			continue
		}
		if slices.ContainsFunc(p.Conns, func(o Conn) bool { return o.Name == c.Name || o.UUID == c.UUID }) {
			p.Warnings = append(p.Warnings, fmt.Sprintf("%s: name or uuid already used (connections.json)", c.Name))
			continue
		}
		// a-b 와 a_b 는 .env 키가 같아 비밀번호를 나눠 쓰게 되므로 뒤의 것을 뺀다(AddConn 의 checkName 과 같은 규칙)
		if slices.ContainsFunc(p.Conns, func(o Conn) bool { return sameSecretKey(o, c) }) {
			p.Warnings = append(p.Warnings, fmt.Sprintf("%s: same .env password key as another connection (connections.json)", c.Name))
			continue
		}
		if err := ValidateConn(c); err != nil {
			p.Warnings = append(p.Warnings, fmt.Sprintf("%s: %v (connections.json)", c.Name, err))
			continue
		}
		c.Password = envPassword(env, c.Kind, c.Name)
		p.Conns = append(p.Conns, c)
	}
	return nil
}

func (e connEntry) conn() Conn {
	return Conn{UUID: e.UUID, Name: e.Name, Group: e.Group, Kind: e.Kind, Host: e.Host, Port: e.Port, DB: e.DB, User: e.User,
		URL: e.URL, Auth: e.Auth, Region: e.Region, Profile: e.Profile, SSLMode: e.SSLMode, Source: SourceDB,
		Workgroup: e.Workgroup, Output: e.Output}
}

func entryOf(c Conn) connEntry {
	return connEntry{UUID: c.UUID, Name: c.Name, Group: c.Group, Kind: c.Kind, Host: c.Host, Port: c.Port, DB: c.DB, User: c.User,
		URL: c.URL, Auth: c.Auth, Region: c.Region, Profile: c.Profile, SSLMode: c.SSLMode,
		Workgroup: c.Workgroup, Output: c.Output}
}

// authOptions 는 종류별로 받는 auth 값이다(R60.1). 첫 값이 접속 창의 기본값이다.
var authOptions = map[Kind][]string{
	KindOpenSearch: {"", "basic", "sigv4"},
	KindLoki:       {"", "basic"},
	KindPrometheus: {"", "basic"},
	KindGrafana:    {"token"},
	KindDynamoDB:   {""},
	KindAthena:     {""},
}

// AuthOptions 는 종류 k 가 받는 auth 값이다. "" 는 인증 없음이다.
func AuthOptions(k Kind) []string { return authOptions[k] }

// checkHTTPURL 은 저장소 URL 이 http(s)://호스트 꼴이고 사용자 정보(비밀번호)가 없는지 본다. 원문은 오류에 싣지 않는다.
func checkHTTPURL(raw string) error {
	u, err := url.Parse(raw)
	switch {
	case err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Hostname() == "":
		return errors.New("url must be http(s)://host")
	case u.User != nil:
		return errors.New("url must not contain user or password (use user and .env)")
	}
	return nil
}

// ParseHTTPURL 은 접속 창·CLI 에 붙인 http(s) URL 을 사용자 정보를 뺀 URL·사용자·비밀번호로 나눈다.
func ParseHTTPURL(raw string) (string, string, string, error) {
	u, err := url.Parse(strings.TrimSpace(raw))
	if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Hostname() == "" {
		return "", "", "", errors.New("invalid url (https://host[:port])")
	}
	user, pw := u.User.Username(), ""
	if u.User != nil {
		pw, _ = u.User.Password()
	}
	u.User = nil
	return strings.TrimSuffix(u.String(), "/"), user, pw, nil
}

// ValidateConn 은 저장할 접속 값을 검사한다. 이름은 영숫자·-·_ 만 받는다(R59.2). 포트 0 은 종류의 기본 포트다.
func ValidateConn(c Conn) error {
	if !ValidConnName(c.Name) {
		return errors.New("name must be letters, digits, - or _")
	}
	if c.Kind.IsSource() {
		return validateSource(c)
	}
	switch {
	case c.Kind != KindPG && c.Kind != KindRedis:
		return fmt.Errorf("kind must be postgresql, redis, opensearch, loki, prometheus, grafana, dynamodb or athena: %q", c.Kind)
	case c.Host == "":
		return errors.New("host is required")
	case c.Port < 0 || c.Port > 65535:
		return fmt.Errorf("invalid port %d", c.Port)
	}
	if c.Kind == KindRedis && c.DB != "" {
		if n, err := strconv.Atoi(c.DB); err != nil || n < 0 {
			return fmt.Errorf("redis db must be a number: %q", c.DB)
		}
	}
	return nil
}

// validateSource 는 읽기 전용 저장소 접속 값을 검사한다(R60.1). HTTP 종류는 url, SigV4·DynamoDB 는 region 이 필요하다.
func validateSource(c Conn) error {
	if !slices.Contains(authOptions[c.Kind], c.Auth) {
		return fmt.Errorf("%s auth must be one of %q: %q", c.Kind, authOptions[c.Kind], c.Auth)
	}
	switch {
	case c.Kind.IsHTTP() && c.URL == "":
		return errors.New("url is required")
	case c.URL != "":
		if err := checkHTTPURL(c.URL); err != nil {
			return err
		}
	}
	if (c.Kind == KindDynamoDB || c.Kind == KindAthena || c.Auth == "sigv4") && c.Region == "" {
		return errors.New("region is required (dynamodb, athena, sigv4)")
	}
	switch {
	case c.Kind == KindAthena && c.URL != "":
		return errors.New("athena does not use url (region, profile, workgroup, output)")
	case c.Kind == KindAthena && c.Output != "" && !strings.HasPrefix(c.Output, "s3://"):
		return errors.New("athena output must be s3://bucket/prefix/")
	}
	return nil
}

// checkName 은 새 이름이 다른 접속(.idea 포함)의 이름·.env 비밀번호 키와 겹치지 않는지 본다. self 는 고치는 접속의 uuid 다.
// a-b 와 a_b 는 .env 키(A_B)가 같아 비밀번호를 서로 덮으므로 막는다.
func checkName(conns []Conn, c Conn, self string) error {
	for _, o := range conns {
		if o.UUID == self {
			continue
		}
		if o.Name == c.Name || o.UUID == c.UUID {
			return fmt.Errorf("connection already exists: %s", c.Name)
		}
		if sameSecretKey(o, c) {
			return fmt.Errorf("%s uses the same .env key as %s", c.Name, o.Name)
		}
	}
	return nil
}

// AddConn 은 db 전용 접속을 connections.json 끝에 더하고 비밀번호가 있으면 .env 에 쓴다. uuid 가 비면 만든다.
// .env 를 먼저 쓴다. connections.json 쓰기가 실패해도 남는 것은 쓰이지 않는 비밀번호 한 줄이다.
func AddConn(dir string, c Conn) (Conn, error) {
	if err := ValidateConn(c); err != nil {
		return c, err
	}
	p, err := LoadProject(dir)
	if err != nil {
		return c, err
	}
	es, err := readConnEntries(dir)
	if err != nil {
		return c, err
	}
	// 겹쳐서 건너뛴 db 전용 접속도 파일에는 있으니 같이 본다
	all := slices.Clone(p.Conns)
	for _, e := range es {
		all = append(all, e.conn())
	}
	if c.UUID == "" {
		c.UUID = newUUID()
	}
	if err := checkName(all, c, ""); err != nil {
		return c, err
	}
	if key := PasswordKey(c.Kind, c.Name); c.Password != "" && key != "" {
		if err := setEnv(dir, []string{key}, key+"="+url.QueryEscape(c.Password)); err != nil {
			return c, err
		}
	}
	c.Source = SourceDB
	return c, writeConnEntries(dir, append(es, entryOf(c)))
}

// UpdateConn 은 db 전용 접속 oldName 을 c 로 바꾼다. uuid 는 그대로 둔다. c.Password 가 비면 비밀번호를 그대로 두고,
// 이름·종류가 바뀌어 .env 키가 달라지면 값을 새 키로 옮긴다(R59.3).
func UpdateConn(dir, oldName string, c Conn) (Conn, error) {
	if err := ValidateConn(c); err != nil {
		return c, err
	}
	p, err := LoadProject(dir)
	if err != nil {
		return c, err
	}
	es, err := readConnEntries(dir)
	if err != nil {
		return c, err
	}
	i := slices.IndexFunc(es, func(e connEntry) bool { return e.Name == oldName })
	if i < 0 {
		return c, dbOnlyErr(oldName)
	}
	old := es[i].conn()
	c.UUID, c.Source = old.UUID, SourceDB
	all := slices.Clone(p.Conns)
	for _, e := range es {
		all = append(all, e.conn())
	}
	if err := checkName(all, c, old.UUID); err != nil {
		return c, err
	}
	// connections.json 을 먼저 쓴다. .env 쓰기가 실패해도 옛 키 줄은 지우지 않아(setEnv 는 바꿔 끼우기라 통째로 실패한다)
	// 비밀번호를 잃지 않고, 무엇을 손으로 고칠지 ErrEnvNotUpdated 로 알린다
	es[i] = entryOf(c)
	if err := writeConnEntries(dir, es); err != nil {
		return c, err
	}
	oldKey, newKey := PasswordKey(old.Kind, old.Name), PasswordKey(c.Kind, c.Name)
	switch {
	case c.Password != "" && newKey != "":
		err = setEnv(dir, []string{oldKey, newKey}, newKey+"="+url.QueryEscape(c.Password))
	case oldKey != newKey && oldKey == "":
		// 비밀값이 없던 종류(DynamoDB)에서 바꾸면 옮길 값이 없다
	case oldKey != newKey:
		// 비밀번호를 그대로 두고 키만 옮긴다. 값은 인코딩된 원문 그대로 옮긴다. 비밀값이 없는 종류로 바꾸면 옛 줄만 지운다
		var raw string
		var found bool
		if raw, found, err = envValue(dir, oldKey); err == nil {
			line := ""
			if found && newKey != "" {
				line = newKey + "=" + raw
			}
			err = setEnv(dir, []string{oldKey, newKey}, line)
		}
	}
	if err != nil {
		return c, fmt.Errorf("%w: move %s to %s by hand: %v", ErrEnvNotUpdated, oldKey, newKey, err)
	}
	return c, nil
}

// ErrEnvNotUpdated 는 UpdateConn 이 connections.json 은 썼지만 .env 비밀번호 줄을 바꾸지 못했을 때다.
// 접속은 저장된 것이라 화면·CLI 는 저장으로 다루고 오류를 그대로 보인다.
var ErrEnvNotUpdated = errors.New("connection saved but .env was not updated")

// RemoveConn 은 db 전용 접속을 connections.json 에서 빼고 .env 비밀번호 줄을 지운다.
func RemoveConn(dir, name string) error {
	es, err := readConnEntries(dir)
	if err != nil {
		return err
	}
	i := slices.IndexFunc(es, func(e connEntry) bool { return e.Name == name })
	if i < 0 {
		return dbOnlyErr(name)
	}
	key := PasswordKey(es[i].Kind, es[i].Name)
	if err := writeConnEntries(dir, slices.Delete(es, i, i+1)); err != nil {
		return err
	}
	if key == "" {
		return nil
	}
	return setEnv(dir, []string{key}, "")
}

func dbOnlyErr(name string) error { return fmt.Errorf("no such db connection: %s", name) }

// newUUID 는 무작위(v4) uuid 다.
func newUUID() string {
	var b [16]byte
	_, _ = rand.Read(b[:]) // 운영체제 난수원이 없을 때만 실패하고, 그때도 uuid 는 겹침 검사(checkName)를 거친다
	b[6] = b[6]&0x0f | 0x40
	b[8] = b[8]&0x3f | 0x80
	return fmt.Sprintf("%x-%x-%x-%x-%x", b[0:4], b[4:6], b[6:8], b[8:10], b[10:16])
}

// ─── .env ───

// envKeyOf 는 .env 한 줄의 키다. KEY=VALUE 가 아니거나 주석이면 빈 글이다(LoadProject 와 같은 규칙).
func envKeyOf(line string) string {
	line = strings.TrimSpace(line)
	k, _, ok := strings.Cut(line, "=")
	if !ok || strings.HasPrefix(line, "#") {
		return ""
	}
	return strings.TrimSpace(k)
}

// envValue 는 .env 에서 key 의 원문 값(인코딩된 채)이다. 같은 키가 여러 줄이면 LoadProject 처럼 마지막 것이다.
func envValue(dir, key string) (string, bool, error) {
	b, err := os.ReadFile(filepath.Join(dir, ".env"))
	if os.IsNotExist(err) {
		return "", false, nil
	}
	if err != nil {
		return "", false, err
	}
	val, found := "", false
	for _, line := range strings.Split(string(b), "\n") {
		if envKeyOf(line) == key {
			_, v, _ := strings.Cut(line, "=")
			val, found = strings.Trim(strings.TrimSpace(v), `"'`), true
		}
	}
	return val, found, nil
}

// setEnv 는 .env 에서 keys 의 줄을 모두 빼고, line 이 있으면 그 첫 줄 자리(없으면 끝)에 둔다. 이름을 바꿔도 비밀번호 줄이 제자리에 남는다.
// 다른 줄·주석·순서는 그대로 둔다. 파일이 없으면 0600 으로 만들고, 있으면 권한을 그대로 둔다(R59.2).
// 심볼릭 링크면 가리키는 파일을 고친다(writeTarget). 파일이 CRLF 줄바꿈이면 새 줄도 CRLF 로 쓴다.
func setEnv(dir string, keys []string, line string) error {
	// 빈 키는 주석·빈 줄(envKeyOf "")과 맞아 그 줄들을 지우게 되므로 뺀다
	keys = slices.DeleteFunc(slices.Clone(keys), func(k string) bool { return k == "" })
	path, perm, err := writeTarget(filepath.Join(dir, ".env"), 0o600)
	if err != nil {
		return err
	}
	b, err := os.ReadFile(path)
	if err != nil && !os.IsNotExist(err) {
		return err
	}
	eol := "\n"
	if bytes.Contains(b, []byte("\r\n")) {
		eol = "\r\n"
	}
	var buf bytes.Buffer
	if len(b) > 0 {
		// 남기는 줄은 끝의 \r 까지 그대로 쓴다
		for _, ln := range strings.Split(strings.TrimSuffix(string(b), "\n"), "\n") {
			switch {
			case !slices.Contains(keys, envKeyOf(ln)):
				buf.WriteString(ln + "\n")
			case line != "":
				buf.WriteString(line + eol)
				line = ""
			}
		}
	}
	if line != "" {
		buf.WriteString(line + eol)
	}
	return atomicfile.Write(path, ".env-*", buf.Bytes(), perm)
}

// ─── URL·시험 ───

// errConnURL 은 접속 URL 을 읽지 못했을 때다. URL 에 비밀번호가 들 수 있어 원문을 싣지 않는다.
var errConnURL = errors.New("invalid connection url (postgres://user:pw@host:5432/db, redis://:pw@host:6379/0)")

// ParseConnURL 은 postgres(ql)://·redis://·rediss:// URL 을 접속 값과 비밀번호로 나눈다. 포트가 없으면 종류의 기본 포트다.
// rediss 의 TLS 는 따로 두지 않는다. Redis 는 평문 응답이 없으면 TLS 로 다시 붙는다(R4.3).
func ParseConnURL(raw string) (Conn, string, error) {
	u, err := url.Parse(strings.TrimSpace(raw))
	if err != nil || u.Host == "" || u.Hostname() == "" || u.Opaque != "" {
		return Conn{}, "", errConnURL
	}
	c := Conn{Host: u.Hostname(), User: u.User.Username(), DB: strings.TrimPrefix(u.Path, "/")}
	switch u.Scheme { // url.Parse 가 스킴을 소문자로 바꾼다
	case "postgres", "postgresql":
		c.Kind = KindPG
	case "redis", "rediss":
		c.Kind = KindRedis
	default:
		return Conn{}, "", errConnURL
	}
	c.Port = DefaultPort(c.Kind)
	if ps := u.Port(); ps != "" {
		n, err := strconv.Atoi(ps)
		if err != nil || n < 1 || n > 65535 {
			return Conn{}, "", errConnURL
		}
		c.Port = n
	}
	if c.Kind == KindRedis && c.DB != "" {
		if _, err := strconv.Atoi(c.DB); err != nil {
			return Conn{}, "", errConnURL
		}
	}
	if c.Kind == KindPG {
		c.SSLMode = u.Query().Get("sslmode")
		if c.SSLMode != "" && !sslModes[c.SSLMode] {
			return Conn{}, "", errConnURL
		}
	}
	pw, _ := u.User.Password()
	return c, pw, nil
}

// sslModes 는 받는 PostgreSQL sslmode 값이다. allow 처럼 잘 안 쓰는 값은 받지 않는다
var sslModes = map[string]bool{"disable": true, "prefer": true, "require": true, "verify-ca": true, "verify-full": true}

// TestConn 은 c(비밀번호 c.Password)로 새로 붙어 서버 버전을 읽고 닫는다. 화면의 접속 풀은 쓰지 않는다.
// 오류에 비밀번호가 나오지 않게 지운다.
func TestConn(ctx context.Context, c Conn) (string, error) {
	cs := NewClients()
	defer cs.Close()
	c.UUID = "test"
	if c.Name == "" {
		c.Name = c.Host
	}
	version, err := testConn(ctx, cs, c)
	if err != nil {
		return "", errors.New(Redact(err.Error(), c.Password))
	}
	return version, nil
}

func testConn(ctx context.Context, cs *Clients, c Conn) (string, error) {
	switch c.Kind {
	case KindPG:
		pg, err := cs.PG(ctx, c)
		if err != nil {
			return "", err
		}
		var v string
		if err := pg.pool.QueryRow(ctx, "show server_version").Scan(&v); err != nil {
			return "", err
		}
		// 16.4 (Debian 16.4-1.pgdg120+1) 처럼 뒤에 붙는 배포판 글은 뺀다
		v, _, _ = strings.Cut(v, " ")
		return "PostgreSQL " + v, nil
	case KindRedis:
		rc, err := cs.Redis(ctx, c)
		if err != nil {
			return "", err
		}
		info, err := rc.cl.Info(ctx, "server").Result()
		if err != nil {
			return "", err
		}
		return redisVersion(info), nil
	}
	// 읽기 전용 저장소는 internal/source 를 알아야 해 main 의 testConn 이 시험한다(db 는 source 를 가져올 수 없다)
	return "", fmt.Errorf("kind must be postgresql or redis: %q", c.Kind)
}

// redisVersion 은 INFO server 글에서 서버 이름과 버전을 고른다. 버전 필드를 숨긴 서버는 이름만 보인다.
func redisVersion(text string) string {
	info, _ := parseInfo(text)
	return cmp.Or(infoVersion(info), "Redis")
}
