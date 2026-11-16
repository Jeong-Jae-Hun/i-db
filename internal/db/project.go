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
	// R60~R63 읽기 전용 저장소. db 전용 접속 파일(R59)에서만 정의한다
	KindOpenSearch Kind = "opensearch"
	KindLoki       Kind = "loki"
	KindPrometheus Kind = "prometheus"
	KindGrafana    Kind = "grafana"
	KindDynamoDB   Kind = "dynamodb"
	KindAthena     Kind = "athena" // R68
)

type Kind string

// SourceKinds 는 R60~R63 읽기 전용 저장소 종류다. 접속 창이 PostgreSQL·Redis 뒤에 이 순서로 돈다.
var SourceKinds = []Kind{KindOpenSearch, KindLoki, KindPrometheus, KindGrafana, KindDynamoDB, KindAthena}

// IsSource 는 읽기 전용 저장소(R60) 종류인지다. 이 종류는 internal/source 의 Backend 로만 다룬다.
func (k Kind) IsSource() bool { return slices.Contains(SourceKinds, k) }

// IsHTTP 는 url 칸이 필요한 HTTP 저장소(OpenSearch·Loki·Prometheus·Grafana)인지다.
func (k Kind) IsHTTP() bool { return k.IsSource() && k != KindDynamoDB && k != KindAthena }

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
	// URL·Auth·Region·Profile 은 R60~R63 저장소용이다. Auth 는 "basic"·"sigv4"·"token"·"" 이고,
	// AWS 자격 증명은 저장하지 않고 Profile(없으면 표준 체인)로 받는다
	URL     string
	Auth    string
	Region  string
	Profile string
	// Workgroup·Output 은 Athena(R68) 작업 그룹과 결과 S3 위치다. 비면 primary·작업 그룹 설정을 따른다
	Workgroup string
	Output    string
	// SSLMode 는 PostgreSQL 접속의 sslmode 를 덮어쓴다(R59.1). 비면 원격 require·로컬 prefer 다
	SSLMode string
	// Folder 는 트리에서 옮긴 폴더다(R66.2, .dbtui/folders.json). 비면 Group 으로 보인다. AES 키는 늘 Group 으로 찾는다
	Folder string
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
	if c.Kind.IsSource() {
		return fmt.Sprintf("%s(%s %s auth=%s region=%s user=%s)", c.Name, c.Kind, redactURL(c.URL), c.Auth, c.Region, c.User)
	}
	return fmt.Sprintf("%s(%s %s:%d/%s user=%s)", c.Name, c.Kind, c.Host, c.Port, c.DB, c.User)
}

// redactURL 은 URL 의 비밀번호 자리를 가린다. ValidateConn 이 사용자 정보가 든 URL 을 막지만 손으로 고친 파일도 찍힐 수 있다.
func redactURL(raw string) string {
	if u, err := url.Parse(raw); err == nil {
		return u.Redacted()
	}
	return "(invalid url)"
}

// hostOf 는 환경 판정에 쓸 호스트다. 읽기 전용 저장소는 Host 가 비어 URL 의 호스트를 본다(R60.2).
func (c Conn) hostOf() string {
	if c.Host == "" && c.URL != "" {
		if u, err := url.Parse(c.URL); err == nil {
			return u.Hostname()
		}
	}
	return c.Host
}

// GoString 은 %#v 도 막는다.
func (c Conn) GoString() string { return c.String() }

// Env 는 접속 환경이다(R2.2·R6). 이름·AWS 프로필에 prod·prd 가 들거나 호스트의 . - 로 나눈 마디가 prod·prd 면 prod 다.
// 호스트·프로필까지 보는 까닭: 이름에 환경이 없는 접속(redis01.prod.…, 프로필 prod-admin 인 DynamoDB)도 prod 로 막아야 안전하다.
// 셋 가운데 하나라도 prod 면 prod 다.
func (c Conn) Env() string {
	// Shop-PROD 도 prod 로 본다(R6.1 안전 쪽). live 는 서비스 이름으로 흔히 쓰여 환경 토큰으로 넣지 않는다
	name, profile := strings.ToLower(c.Name), strings.ToLower(c.Profile)
	hostProd := slices.ContainsFunc(strings.FieldsFunc(strings.ToLower(c.hostOf()), func(r rune) bool { return r == '.' || r == '-' }),
		func(t string) bool { return t == "prod" || t == "prd" })
	switch {
	case strings.Contains(name, "prod"), strings.Contains(name, "prd"), hostProd,
		strings.Contains(profile, "prod"), strings.Contains(profile, "prd"):
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
	p.loadFolders()

	var mp xmlMapping
	if err := readOptionalXML(filepath.Join(dir, ".idea", "data_source_mapping.xml"), &mp); err != nil {
		return nil, err
	}
	for _, f := range mp.Files {
		p.Mapping[strings.TrimPrefix(f.URL, "file://$PROJECT_DIR$/")] = f.Value
	}
	return p, nil
}

// ConnIndex 는 uuid 인 접속의 자리다. 지운 접속 자리(빈 Conn, R59)와 빈 uuid 는 맞지 않는다.
// 매핑이 없는 파일(uuid "")이 지운 자리를 집어 그 접속에 붙지 않게 여기서 막는다.
func (p *Project) ConnIndex(uuid string) (int, bool) {
	if uuid == "" {
		return -1, false
	}
	for i, c := range p.Conns {
		if c.UUID == uuid && c.Kind != "" {
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

// secretPrefix 는 종류별 .env 비밀값 키 접두다(R59.1·R60.1). DynamoDB 는 자격 증명을 profile 로 받아 키가 없다.
var secretPrefix = map[Kind]string{KindPG: "PG_", KindRedis: "REDIS_", KindOpenSearch: "OS_", KindLoki: "LOKI_",
	KindPrometheus: "PROM_", KindGrafana: "GRAFANA_"}

// PasswordKey 는 접속 비밀값을 두는 .env 키다(PG_<이름>_PASSWORD·REDIS_·OS_·LOKI_·PROM_, Grafana 는 GRAFANA_<이름>_TOKEN).
// 비밀값이 없는 종류(DynamoDB·Athena)는 빈 글이다. 모르는 종류는 예전처럼 PG_ 로 둔다.
func PasswordKey(kind Kind, name string) string {
	if kind == KindDynamoDB || kind == KindAthena {
		return ""
	}
	prefix, ok := secretPrefix[kind]
	if !ok {
		prefix = "PG_"
	}
	suffix := "_PASSWORD"
	if kind == KindGrafana {
		suffix = "_TOKEN"
	}
	return prefix + EnvName(name) + suffix
}

// sameSecretKey 는 두 접속이 .env 비밀값 키를 나눠 쓰게 되는지다. 키가 없는 종류는 겹치지 않는다.
func sameSecretKey(a, b Conn) bool {
	k := PasswordKey(a.Kind, a.Name)
	return k != "" && k == PasswordKey(b.Kind, b.Name)
}

// envPassword 는 .env 의 접속 비밀번호다. 값은 URL 인코딩돼 있다(.lazysql.toml 시절 형식). 인코딩이 아닌 원문이면 그대로 쓴다.
func envPassword(env map[string]string, kind Kind, name string) string {
	key := PasswordKey(kind, name)
	if key == "" {
		return ""
	}
	pw := env[key]
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
