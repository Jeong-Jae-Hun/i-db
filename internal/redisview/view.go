package redisview

import (
	"fmt"
	"strconv"
	"strings"

	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

const hints = "e 편집 · a 추가 · d 삭제 · t TTL · r 이름 · D 키 삭제"

var collections = map[string]bool{"hash": true, "list": true, "set": true, "zset": true}

// View 는 머리줄(키·타입·TTL), 값, 입력·안내 줄을 SetSize 크기 안에 그린다.
func (m Model) View() string {
	if m.w <= 0 || m.h <= 0 {
		return ""
	}
	ttl := "∞"
	if m.v.TTL >= 0 {
		ttl = m.v.TTL.String()
	}
	head := fmt.Sprintf("%s · %s · TTL %s", textutil.Printable(m.v.Key, false), m.v.Type, ttl)
	if m.v.Type != "string" && m.rows() > 0 {
		head += fmt.Sprintf(" · %d개", m.rows())
	}
	if m.v.Truncated {
		head += " · 일부만 읽음"
	}
	lines := []string{fit(head, m.w)}

	body := m.bodyHeight()
	end := min(m.rows(), m.top+body)
	switch {
	case m.v.Type == "string":
		for _, l := range m.strLines[m.top:end] {
			lines = append(lines, fit(cellText(l, m.w), m.w))
		}
	case !collections[m.v.Type]:
		lines = append(lines, fit("(편집 미지원 타입: "+textutil.Printable(m.v.Type, false)+")", m.w))
	case m.rows() == 0:
		lines = append(lines, fit("(비어 있음)", m.w))
	default:
		// 왼쪽 칸(hash 필드·list 번호·zset 점수)은 보이는 줄에서 가장 넓은 값에 맞추되 폭의 1/3 을 넘지 않는다
		left := make([]string, end-m.top)
		right := make([]string, end-m.top)
		lw := 0
		for i := m.top; i < end; i++ {
			switch m.v.Type {
			case "hash":
				left[i-m.top], right[i-m.top] = m.v.Fields[i][0], m.v.Fields[i][1]
			case "list":
				left[i-m.top], right[i-m.top] = strconv.Itoa(i), m.v.Items[i]
			case "set":
				right[i-m.top] = m.v.Members[i]
			case "zset":
				left[i-m.top], right[i-m.top] = m.v.ZMembers[i][1], m.v.ZMembers[i][0]
			}
			left[i-m.top] = cellText(left[i-m.top], m.w)
			lw = max(lw, ansi.StringWidth(left[i-m.top]))
		}
		lw = min(lw, m.w/3)
		for i := range left {
			mark := "  "
			if i+m.top == m.sel {
				mark = "▌ "
			}
			row := mark + cellText(right[i], m.w)
			if m.v.Type != "set" {
				row = mark + fit(left[i], lw) + " │ " + cellText(right[i], m.w)
			}
			lines = append(lines, fit(row, m.w))
		}
	}
	for len(lines) < m.h-1 {
		lines = append(lines, "")
	}

	foot := hints
	switch m.op {
	case opNone:
	case opDelete, opDeleteKey:
		foot = m.prompt
	default:
		// 커서 앞은 입력창 폭만큼만 바꿔 1MiB 값도 키마다 전체를 그리지 않는다
		room := max(1, m.w-ansi.StringWidth(m.prompt)-3)
		before := textutil.Printable(string(m.in.buf[max(0, m.in.pos-room):m.in.pos]), false)
		if bw := ansi.StringWidth(before); bw >= room {
			before = ansi.TruncateLeft(before, bw-room+2, "…")
		}
		after := cellText(string(m.in.buf[m.in.pos:min(len(m.in.buf), m.in.pos+room)]), room)
		foot = m.prompt + ": " + before + "▏" + after
	}
	lines = append(lines, fit(foot, m.w))
	return strings.Join(lines[:min(len(lines), m.h)], "\n")
}

// cellText 는 값 하나를 폭 w 한 줄로 그릴 글자로 바꾼다. 값이 클 수 있어 w 칸을 채울 앞부분만 다룬다.
// 한 글자는 4바이트 이하라 4w 바이트면 w 칸을 넘는다(폭 0 결합 문자가 몰리면 덜 보일 수 있다).
func cellText(raw string, w int) string {
	if n := 4*w + 16; len(raw) > n {
		raw = raw[:n]
	}
	return textutil.Printable(strings.ReplaceAll(raw, "\n", "↵"), false)
}

func fit(s string, w int) string {
	if w <= 0 {
		return ""
	}
	t := ansi.Truncate(s, w, "…")
	return t + strings.Repeat(" ", max(0, w-ansi.StringWidth(t)))
}
