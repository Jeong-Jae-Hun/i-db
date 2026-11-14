// Package user360 은 이메일·uid·id·닉네임 하나로 여러 DB 의 사용자 행을 한 번에 찾는 조회 묶음을 만든다(명세 R51).
// 조회 목록은 프로젝트의 .dbtui/user360.json 이고, 없으면 빈 목록(defaultLookups)이다.
// 이 패키지는 SQL·인자만 만든다. 실행(읽기 전용 트랜잭션)과 결과 탭은 화면이 맡는다.
package user360

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"

	"github.com/Jeong-Jae-Hun/i-db/internal/params"
	"github.com/Jeong-Jae-Hun/i-db/internal/sqlfmt"
)

// file 은 프로젝트 폴더 기준 설정 파일 자리다. 테이블·열 이름만 들어 레포에 커밋한다.
const file = ".dbtui/user360.json"

// 입력 종류. classify 가 돌려주는 키이고 Lookup.By 에 쓴다
const (
	kindEmail    = "email"
	kindUID      = "uid"
	kindID       = "id"
	kindNickname = "nickname"
)

// paramEmailEnc 는 이메일을 그 접속 그룹 키로 암호화한 값(staticEncrypt 결과)이다
const paramEmailEnc = "email_enc"

var (
	kinds = []string{kindEmail, kindUID, kindID, kindNickname}
	// SQL 에 쓸 수 있는 :이름. 입력 종류와 email_enc 다
	paramNames = append(slices.Clone(kinds), paramEmailEnc)
	uuidRe     = regexp.MustCompile(`^[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$`)
	digitsRe   = regexp.MustCompile(`^[0-9]+$`)
)

// ErrNoKey 는 Encrypter 가 그 접속의 클라이언트 암호화 키·IV 를 모른다는 뜻이다.
// Plan 은 이때 :email_enc 를 서버 함수 staticEncrypt(<이메일>) 로 바꿔 서버가 암호화하게 한다(R52 와 같은 대체).
var ErrNoKey = errors.New("client-side encryption key or IV is not configured")

// Encrypter 는 conn(접속 이름) 그룹의 키로 plain 을 staticEncrypt 와 같게 암호화한다. 키·IV 가 없으면 ErrNoKey 다.
type Encrypter func(conn, plain string) (string, error)

// Lookup 은 조회 하나다. Conn 은 접속 이름(.idea/dataSources.xml 의 name)이고, By 의 입력 종류 중 하나라도
// 입력과 맞을 때만 돈다. SQL 은 :email·:email_enc·:uid·:id·:nickname 을 쓸 수 있고 입력과 맞지 않은 이름은 NULL 이다.
type Lookup struct {
	Title string   `json:"title"`
	Conn  string   `json:"conn"`
	By    []string `json:"by"`
	SQL   string   `json:"sql"`
}

// Query 는 실행할 문장 하나다. Args[k-1] 은 SQL 의 $k 값(문자열이나 NULL 인 nil)이다.
// Err 가 있으면(암호화 실패 등) 실행하지 말고 그 탭에 오류를 보인다.
type Query struct {
	Title, Conn, SQL string
	Args             []any
	Err              error
}

// defaultLookups 는 설정 파일이 없을 때의 조회 목록이다. 테이블은 프로젝트마다 달라 기본은 비어 있고,
// .dbtui/user360.json 으로 채운다(예: examples/user360.json).
func defaultLookups() []Lookup { return nil }

// Load 는 projectDir 의 설정을 읽는다. 파일이 없으면 defaultLookups 다. 빈 목록([])은 그대로 빈 목록이다.
// 항목마다 제목·접속·종류·SQL 이 있는지, 종류와 :이름이 알려진 것인지 검사하고 틀리면 몇 번째 항목인지 알린다.
func Load(projectDir string) ([]Lookup, error) {
	path := filepath.Join(projectDir, file)
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return defaultLookups(), nil
	}
	if err != nil {
		return nil, err
	}
	var ls []Lookup
	if err := json.Unmarshal(data, &ls); err != nil {
		return nil, fmt.Errorf("%s: %w", file, err)
	}
	for i, l := range ls {
		if err := validate(l); err != nil {
			return nil, fmt.Errorf("%s: lookup %d: %w", file, i+1, err)
		}
	}
	return ls, nil
}

func validate(l Lookup) error {
	switch {
	case strings.TrimSpace(l.Title) == "":
		return errors.New("title is empty")
	case strings.TrimSpace(l.Conn) == "":
		return errors.New("conn is empty")
	case strings.TrimSpace(l.SQL) == "":
		return errors.New("sql is empty")
	case len(l.By) == 0:
		return errors.New("by is empty")
	}
	for _, k := range l.By {
		if !slices.Contains(kinds, k) {
			return fmt.Errorf("unknown kind %q in by (want one of %s)", k, strings.Join(kinds, ", "))
		}
	}
	_, names := params.Bind(l.SQL)
	for _, n := range names {
		if !slices.Contains(paramNames, n) {
			return fmt.Errorf("unknown parameter %s (want :%s)", paramLabel(n), strings.Join(paramNames, ", :"))
		}
	}
	return nil
}

// paramLabel 은 오류에 보일 자리표시다. Bind 는 $1 을 이름 "1" 로 준다
func paramLabel(name string) string {
	if _, err := strconv.Atoi(name); err == nil {
		return "$" + name
	}
	return ":" + name
}

// classify 는 앞뒤 공백을 뗀 입력의 종류다. '@' 가 있으면 email(대소문자는 입력 그대로), 숫자만이고 bigint 에 들면 id,
// uuid 꼴이면 uid(소문자로), 그 밖은 nickname 이다. 빈 입력은 빈 map 이다.
// 이메일을 소문자로 바꾸지 않는 까닭: 암호문은 글자가 하나만 달라도 달라서, 저장한 쪽이 소문자로 맞췄는지 모르는 채 바꾸면
// 맞던 값도 놓친다. 입력한 그대로 찾고, 대소문자가 다르게 저장됐으면 사용자가 그 꼴로 다시 친다.
func classify(input string) map[string]string {
	v := strings.TrimSpace(input)
	_, intErr := strconv.ParseInt(v, 10, 64)
	switch {
	case v == "":
		return map[string]string{}
	case strings.Contains(v, "@"):
		return map[string]string{kindEmail: v}
	case digitsRe.MatchString(v) && intErr == nil:
		return map[string]string{kindID: v}
	case uuidRe.MatchString(v):
		return map[string]string{kindUID: strings.ToLower(v)}
	default:
		return map[string]string{kindNickname: v}
	}
}

// Plan 은 입력 종류와 By 가 겹치는 조회마다 :이름을 $n 으로 바꾼 문장과 인자를 만든다. 입력이 이메일이면
// :email_enc 는 encrypt(접속, 이메일) 값이다. encrypt 가 nil 이거나 ErrNoKey 면 그 자리를 staticEncrypt($n) 으로 감싸고
// 인자로 이메일을 넘긴다. 그 밖의 암호화 오류는 Query.Err 다. 순서는 lookups 순서다.
func Plan(lookups []Lookup, input string, encrypt Encrypter) []Query {
	in := classify(input)
	var out []Query
	for _, l := range lookups {
		if !slices.ContainsFunc(l.By, func(k string) bool { _, ok := in[k]; return ok }) {
			continue
		}
		q := Query{Title: l.Title, Conn: l.Conn}
		sql, names := params.Bind(l.SQL)
		q.Args = make([]any, len(names))
		var server []int // staticEncrypt(...) 로 감쌀 $n
		for k, name := range names {
			if !slices.Contains(paramNames, name) {
				q.Err = fmt.Errorf("unknown parameter %s", paramLabel(name))
				break
			}
			if name != paramEmailEnc {
				if v, ok := in[name]; ok {
					q.Args[k] = v
				}
				continue
			}
			email, ok := in[kindEmail]
			if !ok {
				continue
			}
			var enc string
			err := ErrNoKey
			if encrypt != nil {
				enc, err = encrypt(l.Conn, email)
			}
			switch {
			case errors.Is(err, ErrNoKey):
				q.Args[k] = email
				server = append(server, k+1)
			case err != nil:
				q.Err = fmt.Errorf("encrypt email for %s: %w", l.Conn, err)
			default:
				q.Args[k] = enc
			}
		}
		q.SQL = wrapServer(sql, server)
		if q.Err != nil {
			q.Args = nil
		}
		out = append(out, q)
	}
	return out
}

// wrapServer 는 sql 의 $n(nums 에 든 번호) 자리를 staticEncrypt($n) 으로 바꾼다. 문자열·주석 안은 토큰이 아니라 건드리지 않는다.
func wrapServer(sql string, nums []int) string {
	if len(nums) == 0 {
		return sql
	}
	var b strings.Builder
	last := 0
	for _, t := range sqlfmt.Tokenize(sql) {
		if n, err := strconv.Atoi(strings.TrimPrefix(t.Text, "$")); t.Kind != sqlfmt.Param || err != nil || !slices.Contains(nums, n) {
			continue
		}
		b.WriteString(sql[last:t.Start])
		b.WriteString("staticEncrypt(" + t.Text + ")")
		last = t.End
	}
	b.WriteString(sql[last:])
	return b.String()
}
