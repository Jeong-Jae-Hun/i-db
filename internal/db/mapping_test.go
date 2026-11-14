package db

import (
	"os"
	"path"
	"path/filepath"
	"strings"
	"testing"
	"time"
	"unicode/utf8"
)

// DataGrip 이 쓴 모양 그대로다. 한글 파일 이름과 다른 컴포넌트가 끼어 있다
const realMapping = `<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="DataSourcePerFileMappings">
    <file url="file://$PROJECT_DIR$/queries/auth/Query.sql" value="66666666-7777-4888-9999-aaaaaaaaaaaa" />
    <file url="file://$PROJECT_DIR$/queries/auth/털퇴 조회.sql" value="66666666-7777-4888-9999-aaaaaaaaaaaa" />
    <file url="file://$PROJECT_DIR$/queries/app/Query_1.sql" value="11111111-2222-4333-8444-555555555555" />
  </component>
  <component name="SqlDialectMappings">
    <file url="file://$PROJECT_DIR$/queries/new.sql" value="PostgreSQL" />
  </component>
</project>
`

func writeMapping(t *testing.T, content string, perm os.FileMode) (dir, file string) {
	t.Helper()
	dir = writeFixture(t, false)
	file = filepath.Join(dir, ".idea", "data_source_mapping.xml")
	must(t, os.WriteFile(file, []byte(content), perm))
	must(t, os.Chmod(file, perm))
	return dir, file
}

func readFile(t *testing.T, file string) string {
	t.Helper()
	b, err := os.ReadFile(file)
	must(t, err)
	return string(b)
}

func TestAddMapping_기존_줄은_그대로_두고_컴포넌트_끝에_같은_들여쓰기로_더한다(t *testing.T) {
	dir, file := writeMapping(t, realMapping, 0o640)
	must(t, AddMapping(dir, "queries/etc/app-stg-20261115-090000.sql", "u-pg"))
	want := strings.Replace(realMapping,
		"11111111-2222-4333-8444-555555555555\" />\n  </component>",
		"11111111-2222-4333-8444-555555555555\" />\n"+
			`    <file url="file://$PROJECT_DIR$/queries/etc/app-stg-20261115-090000.sql" value="u-pg" />`+
			"\n  </component>", 1)
	if got := readFile(t, file); got != want {
		t.Fatalf("got\n%s\nwant\n%s", got, want)
	}
	st, err := os.Stat(file)
	must(t, err)
	if st.Mode().Perm() != 0o640 {
		t.Fatalf("권한은 원본을 따른다: %v", st.Mode().Perm())
	}
	p, err := LoadProject(dir)
	must(t, err)
	if p.Mapping["queries/etc/app-stg-20261115-090000.sql"] != "u-pg" || p.Mapping["queries/app/Query_1.sql"] != "11111111-2222-4333-8444-555555555555" {
		t.Fatalf("LoadProject 가 읽는다: %v", p.Mapping)
	}
}

func TestAddMapping_같은_url_이면_값만_바꾸고_같은_값이면_쓰지_않는다(t *testing.T) {
	dir, file := writeMapping(t, realMapping, 0o644)
	must(t, AddMapping(dir, "queries/auth/Query.sql", "u-new"))
	want := strings.Replace(realMapping, `Query.sql" value="66666666-7777-4888-9999-aaaaaaaaaaaa"`, `Query.sql" value="u-new"`, 1)
	if got := readFile(t, file); got != want {
		t.Fatalf("got\n%s\nwant\n%s", got, want)
	}
	old := time.Unix(1_700_000_000, 0)
	must(t, os.Chtimes(file, old, old))
	must(t, AddMapping(dir, "queries/auth/Query.sql", "u-new"))
	if st, _ := os.Stat(file); !st.ModTime().Equal(old) {
		t.Fatal("같은 값이면 파일을 다시 쓰지 않는다")
	}
}

func TestAddMapping_다른_컴포넌트의_같은_url_은_건드리지_않는다(t *testing.T) {
	dir, file := writeMapping(t, realMapping, 0o644)
	must(t, AddMapping(dir, "queries/new.sql", "u-pg"))
	got := readFile(t, file)
	if !strings.Contains(got, `<file url="file://$PROJECT_DIR$/queries/new.sql" value="PostgreSQL" />`) ||
		strings.Count(got, "queries/new.sql") != 2 {
		t.Fatalf("SqlDialectMappings 는 그대로, 매핑에는 새 줄: \n%s", got)
	}
	p, err := LoadProject(dir)
	must(t, err)
	if p.Mapping["queries/new.sql"] == "" {
		t.Fatal("매핑에 들어갔다")
	}
}

func TestAddMapping_파일이_없으면_DataGrip_형식으로_만든다(t *testing.T) {
	dir := writeFixture(t, false)
	file := filepath.Join(dir, ".idea", "data_source_mapping.xml")
	must(t, os.Remove(file))
	must(t, AddMapping(dir, "queries/etc/a.sql", "u-pg"))
	want := "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<project version=\"4\">\n  <component name=\"DataSourcePerFileMappings\">\n" +
		"    <file url=\"file://$PROJECT_DIR$/queries/etc/a.sql\" value=\"u-pg\" />\n  </component>\n</project>\n"
	if got := readFile(t, file); got != want {
		t.Fatalf("got\n%s", got)
	}
	if st, _ := os.Stat(file); st.Mode().Perm() != 0o644 {
		t.Fatalf("새 파일은 0644: %v", st.Mode().Perm())
	}

	noIdea := t.TempDir()
	must(t, AddMapping(noIdea, "queries/etc/a.sql", "u-pg"))
	if got := readFile(t, filepath.Join(noIdea, ".idea", "data_source_mapping.xml")); got != want {
		t.Fatalf(".idea 가 없어도 만든다: %s", got)
	}
}

func TestAddMapping_경로의_XML_특수_글자를_감싸고_다시_읽으면_원래_경로다(t *testing.T) {
	dir, file := writeMapping(t, realMapping, 0o644)
	rel := `queries/etc/a&b <"x">'.sql`
	must(t, AddMapping(dir, rel, "u-pg"))
	if got := readFile(t, file); strings.Contains(got, `a&b <"x">`) {
		t.Fatalf("감싸지 않음:\n%s", got)
	}
	p, err := LoadProject(dir)
	must(t, err)
	if p.Mapping[rel] != "u-pg" {
		t.Fatalf("다시 읽기: %v", p.Mapping)
	}
}

func TestAddMapping_모양이_다른_파일도_원래_바이트를_지킨다(t *testing.T) {
	cases := []struct{ name, in, want string }{
		{
			"CRLF·탭 들여쓰기",
			"<?xml version=\"1.0\"?>\r\n<project version=\"4\">\r\n\t<component name=\"DataSourcePerFileMappings\">\r\n\t\t<file url=\"file://$PROJECT_DIR$/q/a.sql\" value=\"u1\" />\r\n\t</component>\r\n</project>\r\n",
			"<?xml version=\"1.0\"?>\r\n<project version=\"4\">\r\n\t<component name=\"DataSourcePerFileMappings\">\r\n\t\t<file url=\"file://$PROJECT_DIR$/q/a.sql\" value=\"u1\" />\r\n\t\t<file url=\"file://$PROJECT_DIR$/q/n.sql\" value=\"u-pg\" />\r\n\t</component>\r\n</project>\r\n",
		},
		{
			"빈 컴포넌트는 탭 단위로",
			"<project version=\"4\">\n\t<component name=\"DataSourcePerFileMappings\">\n\t</component>\n</project>",
			"<project version=\"4\">\n\t<component name=\"DataSourcePerFileMappings\">\n\t\t<file url=\"file://$PROJECT_DIR$/q/n.sql\" value=\"u-pg\" />\n\t</component>\n</project>",
		},
		{
			"스스로 닫힌 컴포넌트",
			"<project version=\"4\">\n  <component name=\"DataSourcePerFileMappings\" />\n</project>\n",
			"<project version=\"4\">\n  <component name=\"DataSourcePerFileMappings\">\n    <file url=\"file://$PROJECT_DIR$/q/n.sql\" value=\"u-pg\" />\n  </component>\n</project>\n",
		},
		{
			"컴포넌트가 없으면 project 끝에",
			"<project version=\"4\">\n  <component name=\"Other\" />\n</project>\n",
			"<project version=\"4\">\n  <component name=\"Other\" />\n  <component name=\"DataSourcePerFileMappings\">\n    <file url=\"file://$PROJECT_DIR$/q/n.sql\" value=\"u-pg\" />\n  </component>\n</project>\n",
		},
		{
			"한 줄짜리",
			`<project version="4"><component name="DataSourcePerFileMappings"><file url="file://$PROJECT_DIR$/q/a.sql" value="u1"/></component></project>`,
			"<project version=\"4\"><component name=\"DataSourcePerFileMappings\"><file url=\"file://$PROJECT_DIR$/q/a.sql\" value=\"u1\"/>\n  <file url=\"file://$PROJECT_DIR$/q/n.sql\" value=\"u-pg\" />\n</component></project>",
		},
	}
	for _, c := range cases {
		dir, file := writeMapping(t, c.in, 0o644)
		if err := AddMapping(dir, "q/n.sql", "u-pg"); err != nil {
			t.Errorf("%s: %v", c.name, err)
			continue
		}
		if got := readFile(t, file); got != c.want {
			t.Errorf("%s:\ngot  %q\nwant %q", c.name, got, c.want)
			continue
		}
		p, err := LoadProject(dir)
		if err != nil || p.Mapping["q/n.sql"] != "u-pg" {
			t.Errorf("%s: LoadProject %v %v", c.name, p, err)
		}
	}
}

func TestAddMapping_깨진_파일과_프로젝트_밖_경로는_쓰지_않는다(t *testing.T) {
	broken := `<project version="4"><component name="DataSourcePerFileMappings">`
	dir, file := writeMapping(t, broken, 0o644)
	if err := AddMapping(dir, "q/n.sql", "u"); err == nil {
		t.Fatal("깨진 XML 은 오류")
	}
	if readFile(t, file) != broken {
		t.Fatal("깨진 파일을 덮지 않는다")
	}
	for _, rel := range []string{"../x.sql", "/abs/x.sql"} {
		if err := AddMapping(dir, rel, "u"); err == nil {
			t.Errorf("%s: 프로젝트 밖 경로는 오류", rel)
		}
	}
	if err := AddMapping(dir, "q/n.sql", "u"); err == nil {
		t.Fatal("다시 해도 오류")
	}
	other, _ := writeMapping(t, `<module><component name="DataSourcePerFileMappings" /></module>`, 0o644)
	if err := AddMapping(other, "q/n.sql", "u"); err == nil {
		t.Fatal("project 가 아닌 루트는 오류")
	}
}

func TestUniqueQueryPath_그룹_접속_시각으로_짓고_있으면_번호를_붙인다(t *testing.T) {
	dir := t.TempDir()
	now := time.Date(2026, 11, 15, 9, 5, 7, 0, time.UTC)
	cases := []struct{ group, conn, want string }{
		{"app", "app-stg", "queries/app/app-stg-20261115-090507.sql"},
		{"", "app-stg", "queries/etc/app-stg-20261115-090507.sql"},
		{"../x", "a/b c", "queries/x/a-b-c-20261115-090507.sql"},
		{"운영", "회원 조회_1", "queries/운영/회원-조회_1-20261115-090507.sql"},
		{"//", "", "queries/etc/query-20261115-090507.sql"},
	}
	for _, c := range cases {
		got, err := UniqueQueryPath(dir, c.group, c.conn, now)
		if err != nil || got != c.want {
			t.Errorf("(%q, %q) = %q %v, want %q", c.group, c.conn, got, err, c.want)
		}
	}

	first := filepath.Join(dir, "queries", "app", "app-stg-20261115-090507.sql")
	must(t, os.MkdirAll(filepath.Dir(first), 0o755))
	must(t, os.WriteFile(first, nil, 0o644))
	if got, err := UniqueQueryPath(dir, "app", "app-stg", now); err != nil || got != "queries/app/app-stg-20261115-090507-2.sql" {
		t.Fatalf("있으면 -2: %q %v", got, err)
	}
	must(t, os.WriteFile(filepath.Join(dir, "queries", "app", "app-stg-20261115-090507-2.sql"), nil, 0o644))
	if got, err := UniqueQueryPath(dir, "app", "app-stg", now); err != nil || got != "queries/app/app-stg-20261115-090507-3.sql" {
		t.Fatalf("-2 도 있으면 -3: %q %v", got, err)
	}
}

func TestUniqueQueryPath_긴_이름은_파일_이름이_200바이트를_넘지_않게_자른다(t *testing.T) {
	dir := t.TempDir()
	now := time.Date(2026, 11, 15, 9, 5, 7, 0, time.UTC)
	long := strings.Repeat("가", 120) // 360바이트
	got, err := UniqueQueryPath(dir, long, long, now)
	if err != nil {
		t.Fatal(err)
	}
	group, name := path.Split(strings.TrimPrefix(got, "queries/"))
	if len(group)-1 > maxNameBytes || len(name) > maxNameBytes || !utf8.ValidString(got) || !strings.HasSuffix(name, "-20261115-090507.sql") {
		t.Fatalf("잘린 경로: %q (폴더 %d, 파일 %d 바이트)", got, len(group)-1, len(name))
	}
	// 자른 이름도 실제로 만들 수 있다
	must(t, os.MkdirAll(filepath.Join(dir, filepath.Dir(got)), 0o755))
	must(t, os.WriteFile(filepath.Join(dir, got), nil, 0o644))

	c, err := ConflictPath(dir, got, now)
	if err != nil || path.Dir(c) != path.Dir(got) || len(path.Base(c)) > maxNameBytes || !strings.HasSuffix(c, "-conflict-20261115-090507.sql") {
		t.Fatalf("충돌 사본 이름: %q %v", c, err)
	}
	if c, _ := ConflictPath(dir, "queries/a/b.sql", now); c != "queries/a/b-conflict-20261115-090507.sql" {
		t.Fatalf("짧은 이름은 그대로: %q", c)
	}
}
