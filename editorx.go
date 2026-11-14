package main

import (
	"fmt"
	"strconv"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/paramstore"
)

// ─── 편집기 부가: 파라미터 입력 줄 ───

func paramPrompt(p *paramState) string {
	return fmt.Sprintf(":%s (%d/%d) ", p.names[len(p.vals)], len(p.vals)+1, len(p.names))
}

// remembered 는 파라미터 이름이 값을 기억할 이름인지다. $1 같은 번호 자리는 쿼리마다 뜻이 달라 뺀다(R55).
func remembered(name string) bool {
	_, err := strconv.Atoi(name)
	return err != nil
}

// nextParam 은 파라미터 입력 줄을 다음 이름으로 바꾸고 기억한 값을 채운다.
func (m *model) nextParam() {
	p := m.params
	name := p.names[len(p.vals)]
	v := ""
	if remembered(name) {
		v = p.prev[name]
	}
	m.input.Prompt = paramPrompt(p)
	m.input.SetValue(v)
	m.input.CursorEnd()
}

// saveParams 는 성공한 파라미터 실행의 값을 기억하는 명령이다(R55). 값은 개인 상태 폴더에만 둔다.
func (m *model) saveParams(vals map[string]string) tea.Cmd {
	dir := m.proj.Dir
	return func() tea.Msg {
		path, err := paramstore.Path()
		if err == nil {
			err = paramstore.Save(path, dir, vals)
		}
		if err != nil {
			return statusMsg("파라미터 값 기억 실패: " + err.Error())
		}
		return nil
	}
}
