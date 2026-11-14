package user360

import (
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func TestClassify_입력_꼴로_종류를_가른다(t *testing.T) {
	cases := map[string]map[string]string{
		"  a@b.com ":                           {kindEmail: "a@b.com"},
		"1315":                                 {kindID: "1315"},
		"1234567890123456789":                  {kindID: "1234567890123456789"},
		"12345678901234567890":                 {kindNickname: "12345678901234567890"},
		"9223372036854775807":                  {kindID: "9223372036854775807"},
		"9999999999999999999":                  {kindNickname: "9999999999999999999"},
		"A.Kim@Example.com":                    {kindEmail: "A.Kim@Example.com"},
		"6F1C2A3B-4D5E-4F60-8A9B-0C1D2E3F4A5B": {kindUID: "6f1c2a3b-4d5e-4f60-8a9b-0c1d2e3f4a5b"},
		"6f1c2a3b-4d5e-4f60-8a9b-0c1d2e3f4a5":  {kindNickname: "6f1c2a3b-4d5e-4f60-8a9b-0c1d2e3f4a5"},
		"닉네임":                                  {kindNickname: "닉네임"},
		"12a":                                  {kindNickname: "12a"},
		"   ":                                  {},
	}
	for in, want := range cases {
		if got := classify(in); !reflect.DeepEqual(got, want) {
			t.Errorf("%q: got %v want %v", in, got, want)
		}
	}
}

func encFor(conn, plain string) (string, error) { return "ENC[" + conn + ":" + plain + "]", nil }

func TestPlan_종류가_겹치는_조회만_골라_이름을_번호로_바꾼다(t *testing.T) {
	ls := []Lookup{
		{Title: "a", Conn: "auth-prod", By: []string{kindEmail, kindUID}, SQL: "select * from users where email = :email_enc or uid = :uid or :email = 'x'"},
		{Title: "b", Conn: "app-prod", By: []string{kindUID, kindID}, SQL: "select * from t where sub = :uid or user_id = :id or id = :id"},
		{Title: "c", Conn: "app-prod", By: []string{kindNickname}, SQL: "select :nickname"},
	}
	got := Plan(ls, " a@b.com", encFor)
	want := []Query{{Title: "a", Conn: "auth-prod",
		SQL:  "select * from users where email = $1 or uid = $2 or $3 = 'x'",
		Args: []any{"ENC[auth-prod:a@b.com]", nil, "a@b.com"}}}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("이메일:\n got %#v\nwant %#v", got, want)
	}

	got = Plan(ls, "1315", encFor)
	want = []Query{{Title: "b", Conn: "app-prod", SQL: "select * from t where sub = $1 or user_id = $2 or id = $2", Args: []any{nil, "1315"}}}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("id:\n got %#v\nwant %#v", got, want)
	}

	// uid 입력이면 email_enc 는 암호화하지 않고 NULL 이다
	called := false
	got = Plan(ls, "6f1c2a3b-4d5e-4f60-8a9b-0c1d2e3f4a5b", func(string, string) (string, error) { called = true; return "", nil })
	if len(got) != 2 || got[0].Args[0] != nil || got[0].Args[1] != "6f1c2a3b-4d5e-4f60-8a9b-0c1d2e3f4a5b" || called {
		t.Fatalf("uid: %#v called=%v", got, called)
	}
	if got := Plan(ls, "  ", encFor); len(got) != 0 {
		t.Fatalf("빈 입력: %#v", got)
	}
}

func TestPlan_키가_없으면_서버_staticEncrypt_로_감싸고_다른_오류는_탭_오류다(t *testing.T) {
	ls := []Lookup{{Title: "a", Conn: "shop-prod", By: []string{kindEmail},
		SQL: "select ':email_enc', email from users where email = :email_enc or alt = :email_enc -- :email_enc"}}
	wantSQL := "select ':email_enc', email from users where email = staticEncrypt($1) or alt = staticEncrypt($1) -- :email_enc"
	for name, enc := range map[string]Encrypter{
		"nil":      nil,
		"ErrNoKey": func(string, string) (string, error) { return "", ErrNoKey },
	} {
		got := Plan(ls, "a@b.com", enc)
		if len(got) != 1 || got[0].SQL != wantSQL || !reflect.DeepEqual(got[0].Args, []any{"a@b.com"}) || got[0].Err != nil {
			t.Errorf("%s: %#v", name, got)
		}
	}
	boom := errors.New("bad key")
	got := Plan(ls, "a@b.com", func(string, string) (string, error) { return "", boom })
	if len(got) != 1 || !errors.Is(got[0].Err, boom) || got[0].Args != nil {
		t.Fatalf("암호화 오류: %#v", got)
	}
}

// 기본 목록은 비어 있다(테이블은 프로젝트마다 다르다). README 가 안내하는 examples/user360.json 은
// 그대로 .dbtui/user360.json 에 두면 읽히고, 읽기 전용 select … limit 50 이어야 한다.
func TestDefault_는_비어_있고_예시_설정은_검사를_통과하며_읽기_전용_LIMIT_50_이다(t *testing.T) {
	if ls := defaultLookups(); len(ls) != 0 {
		t.Fatalf("기본 조회 수 %d", len(ls))
	}
	data, err := os.ReadFile(filepath.Join("..", "..", "examples", "user360.json"))
	if err != nil {
		t.Fatal(err)
	}
	dir := t.TempDir()
	if err := os.MkdirAll(filepath.Join(dir, ".dbtui"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, file), data, 0o644); err != nil {
		t.Fatal(err)
	}
	ls, err := Load(dir)
	if err != nil || len(ls) == 0 {
		t.Fatalf("예시 설정: %v %v", ls, err)
	}
	for _, l := range ls {
		s := strings.ToLower(l.SQL)
		if !strings.HasPrefix(s, "select ") || !strings.HasSuffix(s, " limit 50") || strings.Contains(s, ";") {
			t.Errorf("%s: 읽기 전용 select … limit 50 이 아니다: %s", l.Title, l.SQL)
		}
	}
	// 이메일·uid·id·닉네임 모두 하나 이상의 조회를 탄다
	for _, in := range []string{"a@b.com", "6f1c2a3b-4d5e-4f60-8a9b-0c1d2e3f4a5b", "1315", "nick"} {
		if len(Plan(ls, in, encFor)) == 0 {
			t.Errorf("%s: 고른 조회가 없다", in)
		}
	}
}

func TestLoad_파일이_없으면_기본이고_있으면_읽고_틀리면_항목을_알린다(t *testing.T) {
	dir := t.TempDir()
	got, err := Load(dir)
	if err != nil || !reflect.DeepEqual(got, defaultLookups()) {
		t.Fatalf("없는 파일: %v %v", got, err)
	}
	write := func(body string) {
		t.Helper()
		if err := os.MkdirAll(filepath.Join(dir, ".dbtui"), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(dir, file), []byte(body), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	write(`[{"title":"x","conn":"auth-stg","by":["email"],"sql":"select * from users where email = :email_enc"}]`)
	got, err = Load(dir)
	want := []Lookup{{Title: "x", Conn: "auth-stg", By: []string{"email"}, SQL: "select * from users where email = :email_enc"}}
	if err != nil || !reflect.DeepEqual(got, want) {
		t.Fatalf("읽기: %v %v", got, err)
	}
	write(`[]`)
	if got, err := Load(dir); err != nil || len(got) != 0 {
		t.Fatalf("빈 목록: %v %v", got, err)
	}

	bad := map[string]string{
		"깨진 JSON": `[{`,
		"제목 없음":   `[{"conn":"c","by":["id"],"sql":"select 1"}]`,
		"접속 없음":   `[{"title":"t","by":["id"],"sql":"select 1"}]`,
		"종류 없음":   `[{"title":"t","conn":"c","sql":"select 1"}]`,
		"모르는 종류":  `[{"title":"t","conn":"c","by":["phone"],"sql":"select 1"}]`,
		"SQL 없음":  `[{"title":"t","conn":"c","by":["id"]}]`,
		"모르는 이름":  `[{"title":"t","conn":"c","by":["id"],"sql":"select :phone"}]`,
		"번호 자리표시": `[{"title":"t","conn":"c","by":["id"],"sql":"select $1"}]`,
	}
	for name, body := range bad {
		write(body)
		_, err := Load(dir)
		if err == nil || !strings.Contains(err.Error(), file) {
			t.Errorf("%s: 파일을 알리는 오류여야 한다: %v", name, err)
		}
	}
	write(`[{"title":"t","conn":"c","by":["id"],"sql":"select 1"},{"title":"t","conn":"c","by":["id"],"sql":"select :phone"}]`)
	if _, err := Load(dir); err == nil || !strings.Contains(err.Error(), "lookup 2") || !strings.Contains(err.Error(), ":phone") {
		t.Errorf("두 번째 항목: %v", err)
	}
}
