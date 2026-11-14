package db

import (
	"bytes"
	"encoding/xml"
	"errors"
	"fmt"
	"io"
	"os"
	"path"
	"path/filepath"
	"strconv"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/Jeong-Jae-Hun/i-db/internal/atomicfile"
)

const (
	mappingComponent = "DataSourcePerFileMappings"
	mappingURLPrefix = "file://$PROJECT_DIR$/"
	// maxQuerySuffix 를 넘도록 같은 이름이 차 있으면 이름 짓기를 그만둔다. 1초에 한 번 만드는 이름이라 닿을 일이 없다
	maxQuerySuffix = 1000
	// maxNameBytes 는 새로 짓는 파일·폴더 이름 한 칸의 상한이다. 흔한 파일 시스템 한계(255바이트)보다 작게 둔다.
	// 긴 접속 이름·충돌 사본(이미 긴 이름 + -conflict-시각)이 그 한계에 걸려 저장이 실패하지 않게 한다
	maxNameBytes    = 200
	queryTimeLayout = "20060102-150405"
)

type xmlSpan struct{ start, end int }

// AddMapping 은 .idea/data_source_mapping.xml 의 DataSourcePerFileMappings 에 relFile(프로젝트 상대 경로) → uuid 를
// 붙인다(R45.1). DataGrip 이 다시 쓸 때 diff 가 커지지 않게 XML 을 다시 찍지 않고 글자로 끼운다.
// 기존 줄·순서·바이트는 그대로 두고, 같은 url 이 있으면 그 항목만 바꾸고, 없으면 컴포넌트 닫는 줄 앞에 같은 들여쓰기로 더한다.
// 파일이 없으면 DataGrip 형식으로 만든다. 같은 폴더 임시 파일에 쓴 뒤 바꿔 끼우고 권한은 원본을 따른다.
func AddMapping(projectDir, relFile, uuid string) error {
	if !filepath.IsLocal(relFile) {
		return fmt.Errorf("mapping path is not inside the project: %s", relFile)
	}
	url := mappingURLPrefix + filepath.ToSlash(relFile)
	file := filepath.Join(projectDir, ".idea", "data_source_mapping.xml")
	data, err := os.ReadFile(file)
	perm := os.FileMode(0o644)
	switch {
	case errors.Is(err, os.ErrNotExist):
		if err := os.MkdirAll(filepath.Dir(file), 0o755); err != nil {
			return err
		}
		data = nil
	case err != nil:
		return err
	default:
		st, err := os.Stat(file)
		if err != nil {
			return err
		}
		perm = st.Mode().Perm()
	}
	out, err := insertMapping(data, url, uuid)
	if err != nil {
		return fmt.Errorf("%s: %w", file, err)
	}
	if bytes.Equal(out, data) {
		return nil
	}
	return atomicfile.Write(file, ".data_source_mapping-*.xml", out, perm)
}

// insertMapping 은 data 에 url → uuid 항목을 넣은 바이트를 돌려준다. 이미 같은 값이면 data 그대로다.
func insertMapping(data []byte, url, uuid string) ([]byte, error) {
	nl := "\n"
	if bytes.Contains(data, []byte("\r\n")) {
		nl = "\r\n"
	}
	entry := `<file url="` + xmlAttr(url) + `" value="` + xmlAttr(uuid) + `" />`
	if len(bytes.TrimSpace(data)) == 0 {
		return []byte(`<?xml version="1.0" encoding="UTF-8"?>` + nl + `<project version="4">` + nl +
			`  <component name="` + mappingComponent + `">` + nl + "    " + entry + nl +
			"  </component>" + nl + "</project>" + nl), nil
	}

	// 깊이 1 = project, 2 = component, 3 = file. 오프셋은 토큰 앞뒤 InputOffset 이다
	d := xml.NewDecoder(bytes.NewReader(data))
	var (
		depth                    int
		inComp, compDone         bool
		comp, projClose, compEnd xmlSpan
		fileStart                int
		matchFile                bool
		matches                  []xmlSpan
		same                     = true
		lastFile                 = -1
		unit                     = "  "
		unitSet                  bool
	)
	for {
		before := int(d.InputOffset())
		tok, err := d.Token()
		if err == io.EOF {
			break
		}
		if err != nil {
			return nil, err
		}
		after := int(d.InputOffset())
		switch t := tok.(type) {
		case xml.StartElement:
			depth++
			switch {
			case depth == 1 && t.Name.Local != "project":
				return nil, fmt.Errorf("root element is %s, not project", t.Name.Local)
			case depth == 2:
				if ind, ok := lineIndent(data, before); ok && !unitSet {
					unit, unitSet = ind, true
				}
				if !compDone && t.Name.Local == "component" && attr(t, "name") == mappingComponent {
					inComp, comp.start = true, before
				}
			case depth == 3 && inComp && t.Name.Local == "file":
				fileStart, lastFile = before, before
				matchFile = attr(t, "url") == url
				if matchFile && attr(t, "value") != uuid {
					same = false
				}
			}
		case xml.EndElement:
			switch {
			case depth == 3 && inComp && matchFile:
				matches = append(matches, xmlSpan{fileStart, after})
				matchFile = false
			case depth == 2 && inComp:
				// 스스로 닫힌 <component … /> 는 닫는 토큰이 입력을 먹지 않아 before == after 다
				compEnd, inComp, compDone = xmlSpan{before, after}, false, true
			case depth == 1:
				projClose = xmlSpan{before, after}
			}
			depth--
		}
	}

	switch {
	case len(matches) > 0 && same:
		return data, nil
	case len(matches) > 0:
		// 같은 url 이 여럿이면 LoadProject 가 마지막 것을 읽으므로 모두 바꾼다
		out := data
		for i := len(matches) - 1; i >= 0; i-- {
			m := matches[i]
			out = append(append(append([]byte{}, out[:m.start]...), entry...), out[m.end:]...)
		}
		return out, nil
	case compDone && compEnd.start == compEnd.end:
		ind, _ := lineIndent(data, comp.start)
		open := bytes.TrimRight(bytes.TrimSuffix(bytes.TrimRight(data[comp.start:compEnd.end], " \t\r\n"), []byte("/>")), " \t")
		repl := string(open) + ">" + nl + ind + unit + entry + nl + ind + "</component>"
		return splice(data, comp.start, compEnd.end, repl), nil
	case compDone:
		ind, ok := lineIndent(data, lastFile)
		if lastFile < 0 || !ok {
			compInd, _ := lineIndent(data, comp.start)
			ind = compInd + unit
		}
		return insertLine(data, compEnd.start, ind+entry, nl), nil
	case projClose.start == projClose.end:
		return nil, errors.New("project element is empty and self-closing")
	default:
		block := unit + `<component name="` + mappingComponent + `">` + nl + unit + unit + entry + nl + unit + "</component>"
		return insertLine(data, projClose.start, block, nl), nil
	}
}

// insertLine 은 at 의 닫는 태그 앞에 line 을 한 줄로 넣는다. 닫는 태그가 줄 처음(공백 뒤)에 있으면 그 줄 앞에 넣고,
// 같은 줄에 다른 글이 있으면 줄을 바꿔 넣은 뒤 닫는 태그를 그 줄 들여쓰기로 둔다.
func insertLine(data []byte, at int, line, nl string) []byte {
	lineStart := bytes.LastIndexByte(data[:at], '\n') + 1
	if strings.TrimLeft(string(data[lineStart:at]), " \t") == "" {
		return splice(data, lineStart, lineStart, line+nl)
	}
	ind, _ := lineIndent(data, lineStart)
	return splice(data, at, at, nl+line+nl+ind)
}

func splice(data []byte, start, end int, s string) []byte {
	out := make([]byte, 0, len(data)-(end-start)+len(s))
	return append(append(append(out, data[:start]...), s...), data[end:]...)
}

// lineIndent 는 at 이 든 줄의 앞 공백이다. at 앞에 공백 아닌 글이 있으면 false 다(그 공백까지만 돌려준다).
func lineIndent(data []byte, at int) (string, bool) {
	if at < 0 {
		return "", false
	}
	lineStart := bytes.LastIndexByte(data[:at], '\n') + 1
	head := data[lineStart:at]
	ind := head[:len(head)-len(bytes.TrimLeft(head, " \t"))]
	return string(ind), len(ind) == len(head)
}

func attr(t xml.StartElement, name string) string {
	for _, a := range t.Attr {
		if a.Name.Local == name {
			return a.Value
		}
	}
	return ""
}

func xmlAttr(s string) string {
	var b strings.Builder
	xml.EscapeText(&b, []byte(s)) // strings.Builder 쓰기는 실패하지 않는다
	return b.String()
}

// UniqueQueryPath 는 새 쿼리를 저장할 프로젝트 상대 경로(R45.1)다. queries/<그룹>/<접속>-<YYYYMMDD-HHmmss>.sql 이고
// 그 파일이 있으면 -2, -3 … 을 붙인다. 그룹·접속 이름은 글자·숫자·-·_ 만 남기고 나머지는 - 로 바꾼다(한글 그대로).
// 단, 고른 뒤 쓰기 전 사이에 다른 프로세스가 같은 이름을 만들면 덮는다. 1초 단위 이름이라 잠그지 않았다.
func UniqueQueryPath(projectDir, group, conn string, now time.Time) (string, error) {
	g := cutBytes(pathSafe(group), maxNameBytes)
	if g == "" {
		g = "etc"
	}
	c := pathSafe(conn)
	if c == "" {
		c = "query"
	}
	return uniquePath(projectDir, path.Join("queries", g), c, "-"+now.Format(queryTimeLayout))
}

// ConflictPath 는 rel(프로젝트 상대 경로) 옆에 둘 충돌 사본 경로 <이름>-conflict-<YYYYMMDD-HHmmss>.sql 이다(R45.3).
// 밖에서 바뀌었거나 지워진 파일 탭의 글을 덮지 않고 남길 때 쓴다. 이름이 있으면 UniqueQueryPath 처럼 번호를 붙인다.
func ConflictPath(projectDir, rel string, now time.Time) (string, error) {
	dir, base := path.Split(filepath.ToSlash(rel))
	return uniquePath(projectDir, path.Clean(dir), strings.TrimSuffix(base, ".sql"), "-conflict-"+now.Format(queryTimeLayout))
}

// uniquePath 는 dir/<name><suffix>.sql 중 아직 없는 경로다. 있으면 -2, -3 … 을 붙인다.
// 번호·확장자까지 붙인 파일 이름이 maxNameBytes 를 넘지 않게 name 을 글자 경계에서 자른다(suffix 의 시각은 남긴다).
func uniquePath(projectDir, dir, name, suffix string) (string, error) {
	tail := fmt.Sprintf("-%d.sql", maxQuerySuffix)
	base := path.Join(dir, cutBytes(name, maxNameBytes-len(suffix)-len(tail))+suffix)
	for n := 1; n <= maxQuerySuffix; n++ {
		rel := base + ".sql"
		if n > 1 {
			rel = base + "-" + strconv.Itoa(n) + ".sql"
		}
		_, err := os.Lstat(filepath.Join(projectDir, filepath.FromSlash(rel)))
		if errors.Is(err, os.ErrNotExist) {
			return rel, nil
		}
		if err != nil {
			return "", err
		}
	}
	return "", fmt.Errorf("too many query files named %s", base)
}

// cutBytes 는 s 를 n 바이트 안으로 UTF-8 글자 경계에서 자른다.
func cutBytes(s string, n int) string {
	if len(s) <= n {
		return s
	}
	for n > 0 && !utf8.RuneStart(s[n]) {
		n--
	}
	return s[:n]
}

// pathSafe 는 파일 이름에 넣을 수 있게 글자·숫자·-·_ 밖의 글자 덩이를 - 하나로 바꾸고 앞뒤 - 를 지운다.
func pathSafe(s string) string {
	var b strings.Builder
	dash := false
	for _, r := range s {
		if unicode.IsLetter(r) || unicode.IsDigit(r) || r == '-' || r == '_' {
			b.WriteRune(r)
			dash = false
			continue
		}
		if !dash {
			b.WriteByte('-')
			dash = true
		}
	}
	return strings.Trim(b.String(), "-")
}
