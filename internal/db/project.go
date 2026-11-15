package db

import (
	"encoding/xml"
	"fmt"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"sort"
	"strconv"
	"strings"
)

var (
	nonAlnum = regexp.MustCompile(`[^A-Za-z0-9]`)
	jdbcRe   = regexp.MustCompile(`^jdbc:\w+://([^:/@?]+)(?::(\d+))?(?:/([^?]*))?(?:\?.*)?$`)
)

const (
	KindPG    Kind = "postgresql"
	KindRedis Kind = "redis"
)

type Kind string

type Conn struct {
	Name     string
	UUID     string
	Group    string
	Kind     Kind
	Host     string
	Port     int
	DB       string
	User     string
	Password string
	// Source 는 접속을 읽은 곳이다. SourceDB 면 .dbtui/connections.json(R59), 비면 DataGrip .idea 다
	Source string
}

// SourceDB 는 db 전용 접속 파일에서 읽은 접속 표시다(R59.1).
const SourceDB = "db"

type Project struct {
	Dir      string
	Conns    []Conn
	Mapping  map[string]string // queries 상대 경로 → 접속 uuid
	Warnings []string          // 읽지 못하고 건너뛴 접속
	// .env 의 <그룹>_AES_KEY·<그룹>_AES_IV 값(R37 복호화 보기·R52 enc()). 키 이름 → 값. 비밀값이라 상태 줄·기록·로그에 내지 않는다
	AESEnv map[string]string
}

type xmlDataSources struct {
	Sources []struct {
		Name    string `xml:"name,attr"`
		UUID    string `xml:"uuid,attr"`
		Group   string `xml:"group,attr"`
		Driver  string `xml:"driver-ref"`
		JDBCURL string `xml:"jdbc-url"`
		User    string `xml:"user-name"`
	} `xml:"component>data-source"`
}

type xmlMapping struct {
	Files []struct {
		URL   string `xml:"url,attr"`
		Value string `xml:"value,attr"`
	} `xml:"component>file"`
}

// String 은 %v·%+v 로 찍혀도 비밀번호가 나오지 않게 한다.
func (c Conn) String() string {
	return fmt.Sprintf("%s(%s %s:%d/%s user=%s)", c.Name, c.Kind, c.Host, c.Port, c.DB, c.User)
}

// GoString 은 %#v 도 막는다.
func (c Conn) GoString() string { return c.String() }

// Env 는 접속 환경이다(R2.2·R6). 이름에 prod·prd 가 들거나 호스트의 . - 로 나눈 마디가 prod·prd 면 prod 다.
// 호스트까지 보는 까닭: 이름에 환경이 없는 접속(redis01.prod.… 같은)도 prod 로 막아야 안전하다.
func (c Conn) Env() string {
	// Shop-PROD 도 prod 로 본다(R6.1 안전 쪽). live 는 서비스 이름으로 흔히 쓰여 환경 토큰으로 넣지 않는다
	name := strings.ToLower(c.Name)
	hostProd := slices.ContainsFunc(strings.FieldsFunc(strings.ToLower(c.Host), func(r rune) bool { return r == '.' || r == '-' }),
		func(t string) bool { return t == "prod" || t == "prd" })
	switch {
	case strings.Contains(name, "prod"), strings.Contains(name, "prd"), hostProd:
		return "prod"
	case strings.Contains(name, "stg"):
		return "stg"
	default:
		return "local"
	}
}

func LoadProject(dir string) (*Project, error) {
	dsPath := filepath.Join(dir, ".idea", "dataSources.xml")
	var ds xmlDataSources
	if err := readXML(dsPath, &ds); err != nil {
		if os.IsNotExist(err) {
			return nil, fmt.Errorf("dataSources.xml not found: %s", dsPath)
		}
		return nil, fmt.Errorf("read %s: %w", dsPath, err)
	}

	var local xmlDataSources
	if err := readOptionalXML(filepath.Join(dir, ".idea", "dataSources.local.xml"), &local); err != nil {
		return nil, err
	}
	users := map[string]string{}
	for _, s := range local.Sources {
		users[s.UUID] = s.User
	}

	// .env: KEY=VALUE 줄만 읽는다. 없으면 비밀번호 없이 진행한다(명세 R1.1).
	env := map[string]string{}
	envPath := filepath.Join(dir, ".env")
	if b, err := os.ReadFile(envPath); err == nil {
		for _, line := range strings.Split(string(b), "\n") {
			line = strings.TrimSpace(line)
			k, v, ok := strings.Cut(line, "=")
			if !ok || strings.HasPrefix(line, "#") {
				continue
			}
			env[strings.TrimSpace(k)] = strings.Trim(strings.TrimSpace(v), `"'`)
		}
	} else if !os.IsNotExist(err) {
		return nil, fmt.Errorf("read %s: %w", envPath, err)
	}

	p := &Project{Dir: dir, Mapping: map[string]string{}, AESEnv: map[string]string{}}
	for k, v := range env {
		if strings.HasSuffix(k, "_AES_KEY") || strings.HasSuffix(k, "_AES_IV") {
			p.AESEnv[k] = v
		}
	}
	for _, s := range ds.Sources {
		kind := Kind(s.Driver)
		if kind != KindPG && kind != KindRedis {
			continue
		}
		m := jdbcRe.FindStringSubmatch(s.JDBCURL)
		if m == nil {
			p.Warnings = append(p.Warnings, fmt.Sprintf("%s: unsupported jdbc url", s.Name))
			continue
		}
		port := 0
		if m[2] != "" {
			n, err := strconv.Atoi(m[2])
			if err != nil || n < 1 || n > 65535 {
				p.Warnings = append(p.Warnings, fmt.Sprintf("%s: invalid port %q", s.Name, m[2]))
				continue
			}
			port = n
		}
		p.Conns = append(p.Conns, Conn{Name: s.Name, UUID: s.UUID, Group: s.Group, Kind: kind,
			Host: m[1], Port: port, DB: m[3], User: users[s.UUID], Password: envPassword(env, kind, s.Name)})
	}
	if err := p.loadDBConns(env); err != nil {
		return nil, err
	}

	var mp xmlMapping
	if err := readOptionalXML(filepath.Join(dir, ".idea", "data_source_mapping.xml"), &mp); err != nil {
		return nil, err
	}
	for _, f := range mp.Files {
		p.Mapping[strings.TrimPrefix(f.URL, "file://$PROJECT_DIR$/")] = f.Value
	}
	return p, nil
}

func (p *Project) ConnIndex(uuid string) (int, bool) {
	for i, c := range p.Conns {
		if c.UUID == uuid {
			return i, true
		}
	}
	return -1, false
}

// Groups 는 그룹 이름 순, 그룹 안은 DataGrip 파일 순서를 유지한다. 그룹 없는 접속은 "기타".
func (p *Project) Groups() []string {
	seen := map[string]bool{}
	var gs []string
	for _, c := range p.Conns {
		g := GroupOf(c)
		if !seen[g] {
			seen[g] = true
			gs = append(gs, g)
		}
	}
	sort.Strings(gs)
	return gs
}

// AESKeyIV 는 접속 c 그룹의 클라이언트 암호화 키·IV 다(R37·R52). 그룹이 없거나 .env 에 없으면 빈 글이다.
func (p *Project) AESKeyIV(c Conn) (key, iv string) {
	if c.Group == "" {
		return "", ""
	}
	n := EnvName(c.Group)
	return p.AESEnv[n+"_AES_KEY"], p.AESEnv[n+"_AES_IV"]
}

func GroupOf(c Conn) string {
	if c.Group == "" {
		return "기타"
	}
	return c.Group
}

// PasswordKey 는 접속 비밀번호를 두는 .env 키다(PG_<이름>_PASSWORD·REDIS_<이름>_PASSWORD).
func PasswordKey(kind Kind, name string) string {
	prefix := "PG_"
	if kind == KindRedis {
		prefix = "REDIS_"
	}
	return prefix + EnvName(name) + "_PASSWORD"
}

// envPassword 는 .env 의 접속 비밀번호다. 값은 URL 인코딩돼 있다(.lazysql.toml 시절 형식). 인코딩이 아닌 원문이면 그대로 쓴다.
func envPassword(env map[string]string, kind Kind, name string) string {
	pw := env[PasswordKey(kind, name)]
	if dec, err := url.QueryUnescape(pw); err == nil {
		pw = dec
	}
	return pw
}

func EnvName(name string) string {
	return strings.ToUpper(nonAlnum.ReplaceAllString(name, "_"))
}

func readXML(path string, v any) error {
	f, err := os.Open(path)
	if err != nil {
		return err
	}
	defer f.Close()
	return xml.NewDecoder(f).Decode(v)
}

// readOptionalXML 은 파일이 없으면 빈 값으로 두고, 있는데 깨졌으면 오류를 돌려준다.
func readOptionalXML(path string, v any) error {
	if err := readXML(path, v); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("read %s: %w", path, err)
	}
	return nil
}
