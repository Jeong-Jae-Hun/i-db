package db

import (
	"bytes"
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

const (
	DefaultPGPort    = 5432
	DefaultRedisPort = 6379
)

var connNameRe = regexp.MustCompile(`^[A-Za-z0-9_-]+$`)

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
}

// ConnsPath 는 db 전용 접속 파일 경로다.
func ConnsPath(dir string) string { return filepath.Join(dir, ".dbtui", "connections.json") }

func readConnEntries(dir string) ([]connEntry, error) {
	b, err := os.ReadFile(ConnsPath(dir))
	if os.IsNotExist(err) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", ConnsPath(dir), err)
	}
	var es []connEntry
	if err := json.Unmarshal(b, &es); err != nil {
		return nil, fmt.Errorf("read %s: %w", ConnsPath(dir), err)
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
	if err := os.MkdirAll(filepath.Dir(ConnsPath(dir)), 0o755); err != nil {
		return err
	}
	perm := os.FileMode(0o644)
	if st, err := os.Stat(ConnsPath(dir)); err == nil {
		perm = st.Mode().Perm()
	}
	return atomicfile.Write(ConnsPath(dir), ".connections-*.json", append(b, '\n'), perm)
}

// loadDBConns 는 .idea 접속 뒤에 db 전용 접속을 붙인다. 이름·uuid 가 .idea 접속과 겹치면 빼고 경고한다(R59.1).
func (p *Project) loadDBConns(env map[string]string) error {
	es, err := readConnEntries(p.Dir)
	if err != nil {
		return err
	}
	for _, e := range es {
		c := e.conn()
		if slices.ContainsFunc(p.Conns, func(o Conn) bool { return o.Name == c.Name || o.UUID == c.UUID }) {
			p.Warnings = append(p.Warnings, fmt.Sprintf("%s: name or uuid already used (connections.json)", c.Name))
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
	return Conn{UUID: e.UUID, Name: e.Name, Group: e.Group, Kind: e.Kind, Host: e.Host, Port: e.Port, DB: e.DB, User: e.User, Source: SourceDB}
}

func entryOf(c Conn) connEntry {
	return connEntry{UUID: c.UUID, Name: c.Name, Group: c.Group, Kind: c.Kind, Host: c.Host, Port: c.Port, DB: c.DB, User: c.User}
}

// ValidateConn 은 저장할 접속 값을 검사한다. 이름은 영숫자·-·_ 만 받는다(R59.2). 포트 0 은 종류의 기본 포트다.
func ValidateConn(c Conn) error {
	switch {
	case !connNameRe.MatchString(c.Name):
		return errors.New("name must be letters, digits, - or _")
	case c.Kind != KindPG && c.Kind != KindRedis:
		return fmt.Errorf("kind must be postgresql or redis: %q", c.Kind)
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
		if PasswordKey(o.Kind, o.Name) == PasswordKey(c.Kind, c.Name) {
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
	if c.Password != "" {
		key := PasswordKey(c.Kind, c.Name)
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
		return c, dbOnlyErr(p, oldName)
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
	oldKey, newKey := PasswordKey(old.Kind, old.Name), PasswordKey(c.Kind, c.Name)
	switch {
	case c.Password != "":
		err = setEnv(dir, []string{oldKey, newKey}, newKey+"="+url.QueryEscape(c.Password))
	case oldKey != newKey:
		// 비밀번호를 그대로 두고 키만 옮긴다. 값은 인코딩된 원문 그대로 옮긴다
		var raw string
		var found bool
		if raw, found, err = envValue(dir, oldKey); err == nil {
			line := ""
			if found {
				line = newKey + "=" + raw
			}
			err = setEnv(dir, []string{oldKey, newKey}, line)
		}
	}
	if err != nil {
		return c, err
	}
	es[i] = entryOf(c)
	return c, writeConnEntries(dir, es)
}

// RemoveConn 은 db 전용 접속을 connections.json 에서 빼고 .env 비밀번호 줄을 지운다.
func RemoveConn(dir, name string) error {
	es, err := readConnEntries(dir)
	if err != nil {
		return err
	}
	i := slices.IndexFunc(es, func(e connEntry) bool { return e.Name == name })
	if i < 0 {
		p, err := LoadProject(dir)
		if err != nil {
			return err
		}
		return dbOnlyErr(p, name)
	}
	key := PasswordKey(es[i].Kind, es[i].Name)
	if err := writeConnEntries(dir, slices.Delete(es, i, i+1)); err != nil {
		return err
	}
	return setEnv(dir, []string{key}, "")
}

// ErrDataGripConn 은 .idea 접속을 고치거나 지우려 할 때의 오류다. 화면·CLI 는 R59.3 안내로 바꿔 보인다.
var ErrDataGripConn = errors.New("DataGrip connections are edited in DataGrip")

func dbOnlyErr(p *Project, name string) error {
	if slices.ContainsFunc(p.Conns, func(c Conn) bool { return c.Name == name && c.Source != SourceDB }) {
		return ErrDataGripConn
	}
	return fmt.Errorf("no such db connection: %s", name)
}

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
func setEnv(dir string, keys []string, line string) error {
	path := filepath.Join(dir, ".env")
	perm := os.FileMode(0o600)
	b, err := os.ReadFile(path)
	switch {
	case err == nil:
		st, err := os.Stat(path)
		if err != nil {
			return err
		}
		perm = st.Mode().Perm()
	case !os.IsNotExist(err):
		return err
	}
	var buf bytes.Buffer
	if len(b) > 0 {
		for _, ln := range strings.Split(strings.TrimSuffix(string(b), "\n"), "\n") {
			switch {
			case !slices.Contains(keys, envKeyOf(ln)):
				buf.WriteString(ln + "\n")
			case line != "":
				buf.WriteString(line + "\n")
				line = ""
			}
		}
	}
	if line != "" {
		buf.WriteString(line + "\n")
	}
	return atomicfile.Write(path, ".env-*", buf.Bytes(), perm)
}

// ─── URL·시험 ───

// ErrConnURL 은 접속 URL 을 읽지 못했을 때다. URL 에 비밀번호가 들 수 있어 원문을 싣지 않는다.
var ErrConnURL = errors.New("invalid connection url (postgres://user:pw@host:5432/db, redis://:pw@host:6379/0)")

// ParseConnURL 은 postgres(ql)://·redis://·rediss:// URL 을 접속 값과 비밀번호로 나눈다. 포트가 없으면 종류의 기본 포트다.
// rediss 의 TLS 는 따로 두지 않는다. Redis 는 평문 응답이 없으면 TLS 로 다시 붙는다(R4.3).
func ParseConnURL(raw string) (Conn, string, error) {
	u, err := url.Parse(strings.TrimSpace(raw))
	if err != nil || u.Host == "" || u.Hostname() == "" || u.Opaque != "" {
		return Conn{}, "", ErrConnURL
	}
	c := Conn{Host: u.Hostname(), User: u.User.Username(), DB: strings.TrimPrefix(u.Path, "/")}
	switch u.Scheme {
	case "postgres", "postgresql":
		c.Kind, c.Port = KindPG, DefaultPGPort
	case "redis", "rediss":
		c.Kind, c.Port = KindRedis, DefaultRedisPort
	default:
		return Conn{}, "", ErrConnURL
	}
	if ps := u.Port(); ps != "" {
		n, err := strconv.Atoi(ps)
		if err != nil || n < 1 || n > 65535 {
			return Conn{}, "", ErrConnURL
		}
		c.Port = n
	}
	if c.Kind == KindRedis && c.DB != "" {
		if _, err := strconv.Atoi(c.DB); err != nil {
			return Conn{}, "", ErrConnURL
		}
	}
	pw, _ := u.User.Password()
	return c, pw, nil
}

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
	return "", fmt.Errorf("kind must be postgresql or redis: %q", c.Kind)
}

// redisVersion 은 INFO server 에서 서버 이름과 버전을 고른다. Valkey 는 호환용 redis_version 도 내므로 valkey_version 을 먼저 본다.
func redisVersion(info string) string {
	fields := map[string]string{}
	for _, line := range strings.Split(info, "\n") {
		if k, v, ok := strings.Cut(strings.TrimSpace(line), ":"); ok {
			fields[k] = v
		}
	}
	if v := fields["valkey_version"]; v != "" {
		return "Valkey " + v
	}
	return "Redis " + fields["redis_version"]
}
