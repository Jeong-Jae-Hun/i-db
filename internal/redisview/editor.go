package redisview

import (
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"math"
	"slices"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

const (
	// MaxStringEdit 는 편집기로 고칠 수 있는 값의 최대 바이트다. GetValue 가 string 을 앞 1MiB 까지만 읽어
	// 그보다 큰 값을 고치면 뒷부분이 사라진다
	MaxStringEdit = 1 << 20
	// list 항목 삭제 표식의 접두어. Redis 에 인덱스로 지우는 명령이 없어 표식으로 바꾼 뒤 그 표식 하나를 LREM 한다
	sentinelPrefix = "__db_deleted_"
	pageRows       = 10
	// list 편집·삭제는 읽은 뒤 다른 클라이언트가 list 를 바꾸면 인덱스가 다른 칸을 가리킨다. 그 칸이 아직
	// 읽은 값일 때만 바꾸도록 확인과 쓰기를 스크립트 하나로 묶는다. ARGV: 인덱스, 읽은 값, 새 값(삭제는 표식)
	listSetScript = "if redis.call('LINDEX',KEYS[1],ARGV[1])==ARGV[2] then " +
		"return redis.call('LSET',KEYS[1],ARGV[1],ARGV[3]) " +
		"else return redis.error_reply('list changed since it was read') end"
	listDeleteScript = "if redis.call('LINDEX',KEYS[1],ARGV[1])==ARGV[2] then " +
		"redis.call('LSET',KEYS[1],ARGV[1],ARGV[3]) return redis.call('LREM',KEYS[1],1,ARGV[3]) " +
		"else return redis.error_reply('list changed since it was read') end"
)

const (
	opNone op = iota
	opEdit
	opAddFirst // hash 필드·zset 멤버를 받는 첫 단계
	opAdd
	opTTL
	opRename
	opDelete
	opDeleteKey
)

var (
	errTooLarge = fmt.Errorf("value is larger than %d bytes", MaxStringEdit)
	errTTL      = errors.New("TTL must be positive seconds, or empty/-1 to persist")
)

type op int

// Value 는 편집할 키 하나다. 내용은 잘리지 않은 원문이어야 한다(화면용으로 64KiB 에서 자른 칸을 넣으면
// 편집할 때 잘린 값이 저장된다). Truncated 는 string 을 앞부분만 읽었거나 컬렉션을 일부만 읽었다는 뜻이다.
type Value struct {
	Key       string
	Type      string        // string·hash·list·set·zset. 그 밖은 TTL·이름·키 삭제만 된다
	TTL       time.Duration // 음수면 만료 없음
	String    string
	Fields    [][2]string // hash: 필드, 값
	Items     []string    // list: LRANGE 0 부터의 순서
	Members   []string    // set
	ZMembers  [][2]string // zset: 멤버, 점수
	Truncated bool
}

// Action 은 확정된 편집의 결과다. Run 은 차례로 실행할 Redis 명령 인자이고, 쓰기라 통합이 prod 확인을 거쳐
// 같은 세션에서 첫 오류까지 실행한다. Key 는 실행 뒤 다시 읽을 키(이름을 바꾸면 새 이름, 키를 지우면 빈 값)다.
type Action struct {
	Run    [][]string
	Key    string
	Status string
}

// Model 은 값 창의 편집 모델이다. 실행 결과를 스스로 반영하지 않고, 통합이 다시 읽은 값으로 새로 만든다.
type Model struct {
	v        Value
	strLines []string // string 값의 줄. 1MiB 를 매 프레임 나누지 않으려고 한 번만 만든다
	sel, top int
	w, h     int

	op     op
	prompt string
	in     input
	first  string // 두 단계 입력의 첫 값(hash 필드, zset 멤버)
}

// input 은 한 줄 입력이다. bubbles textinput 은 줄바꿈·탭을 공백으로 바꿔 값이 몰래 바뀌므로 룬을 그대로 둔다.
type input struct {
	buf []rune
	pos int
}

// edit 은 명령 하나를 만들 재료다. target 은 고를 대상(hash 필드, set·zset 멤버), old 는 바꾸기 전 값이다.
type edit struct {
	op      op
	typ     string
	key     string
	index   int
	target  string
	old     string
	text    string
	keepTTL bool
}

// NewEditor 는 값 하나의 편집 모델을 만든다.
func NewEditor(v Value) Model {
	m := Model{v: v}
	if v.Type == "string" {
		m.strLines = strings.Split(v.String, "\n")
	}
	return m
}

// Editing 이 참이면 입력·확인 중이라 통합은 모든 키를 이 모델에 넘겨야 한다.
func (m Model) Editing() bool { return m.op != opNone }

// Confirming 은 항목·키 지우기의 y/N 확인 중인지다. 이때는 y 만 지우고 다른 키는 취소다.
func (m Model) Confirming() bool { return m.op == opDelete || m.op == opDeleteKey }

// Type 은 편집 중인 키의 Redis 타입이다(string·hash·list·set·zset 등).
func (m Model) Type() string { return m.v.Type }

// SetSize 는 그릴 크기를 정한다.
func (m Model) SetSize(w, h int) Model {
	m.w, m.h = w, h
	return m.clamp()
}

// Update 는 키 하나를 처리한다. 확정되지 않은 키는 빈 Action 이다.
func (m Model) Update(msg tea.KeyMsg) (Model, Action) {
	key := msg.String()
	switch m.op {
	case opNone:
	case opDelete, opDeleteKey:
		e := m.draft("")
		m.op = opNone
		if key != "y" {
			return m, Action{Status: "취소"}
		}
		return m, m.act(e)
	default:
		switch key {
		case "esc":
			m.op = opNone
			return m, Action{Status: "취소"}
		case "enter":
			text := string(m.in.buf)
			if m.op == opAddFirst {
				m.first, m.op, m.in = text, opAdd, input{}
				m.prompt = "값"
				if m.v.Type == "zset" {
					m.prompt = "점수"
				}
				return m, Action{}
			}
			e := m.draft(text)
			m.op = opNone
			return m, m.act(e)
		}
		m.in = m.in.update(msg)
		return m, Action{}
	}

	switch key {
	case "up", "k":
		m.sel--
	case "down", "j":
		m.sel++
	case "pgup":
		m.sel -= pageRows
	case "pgdown":
		m.sel += pageRows
	case "home", "g":
		m.sel = 0
	case "end", "G":
		m.sel = m.rows() - 1
	case "e", "enter":
		old, err := m.selected()
		if err != nil {
			return m, Action{Status: err.Error()}
		}
		return m.start(opEdit, old), Action{}
	case "a":
		switch m.v.Type {
		case "hash", "zset":
			return m.start(opAddFirst, ""), Action{}
		case "list", "set":
			return m.start(opAdd, ""), Action{}
		}
		return m, Action{Status: fmt.Sprintf("%s: add not supported", m.v.Type)}
	case "d":
		if m.v.Type == "string" {
			return m, Action{Status: "string 은 D 로 키를 지운다"}
		}
		// 지우기는 값을 입력 버퍼로 옮기지 않으므로 바이너리·큰 값도 고른 칸만 있으면 된다
		switch m.v.Type {
		case "hash", "list", "set", "zset":
		default:
			return m, Action{Status: fmt.Sprintf("%s: delete not supported", m.v.Type)}
		}
		if m.rows() == 0 {
			return m, Action{Status: "no item selected"}
		}
		return m.start(opDelete, ""), Action{}
	case "t":
		ttl := ""
		if m.v.TTL >= 0 {
			ttl = strconv.FormatInt(int64(m.v.TTL/time.Second), 10)
		}
		return m.start(opTTL, ttl), Action{}
	case "r":
		return m.start(opRename, m.v.Key), Action{}
	case "D":
		return m.start(opDeleteKey, ""), Action{}
	}
	return m.clamp(), Action{}
}

// start 는 입력이나 y/N 확인을 시작한다. 프롬프트는 op 와 타입으로 정한다.
func (m Model) start(o op, initial string) Model {
	m.op, m.first = o, ""
	m.in = input{buf: []rune(initial), pos: utf8.RuneCountInString(initial)}
	switch o {
	case opEdit:
		m.prompt = "새 값"
		if m.v.Type == "zset" {
			m.prompt = "새 점수"
		}
	case opAddFirst:
		m.prompt = "새 필드"
		if m.v.Type == "zset" {
			m.prompt = "새 멤버"
		}
	case opAdd:
		m.prompt = "추가할 값"
	case opTTL:
		m.prompt = "TTL 초(비우거나 -1 이면 만료 없음)"
	case opRename:
		m.prompt = "새 이름(이미 있으면 바꾸지 않음)"
	case opDelete:
		m.prompt = "고른 항목을 지울까요? y/N"
	case opDeleteKey:
		m.prompt = "키 " + textutil.Printable(m.v.Key, false) + " 를 지울까요? y/N"
	}
	return m
}

// selected 는 고른 항목의 현재 값(편집 입력의 처음 값)이다. 고칠 수 없는 값이면 이유를 오류로 준다.
func (m Model) selected() (string, error) {
	var old string
	switch m.v.Type {
	case "string":
		if m.v.Truncated {
			return "", errors.New("string was read partially; edit refused")
		}
		old = m.v.String
	case "hash", "list", "set", "zset":
		if m.rows() == 0 {
			return "", errors.New("no item selected")
		}
		switch m.v.Type {
		case "hash":
			old = m.v.Fields[m.sel][1]
		case "list":
			old = m.v.Items[m.sel]
		case "set":
			old = m.v.Members[m.sel]
		case "zset":
			old = m.v.ZMembers[m.sel][1]
		}
	default:
		return "", fmt.Errorf("%s: edit not supported", m.v.Type)
	}
	if len(old) > MaxStringEdit {
		return "", errTooLarge
	}
	// 룬 버퍼로 옮기면 잘못된 UTF-8 바이트가 U+FFFD 로 바뀌어 저장 때 값이 깨진다
	if !utf8.ValidString(old) {
		return "", errors.New("binary value; edit refused")
	}
	return old, nil
}

// draft 는 현재 고른 항목과 입력으로 명령 재료를 만든다.
func (m Model) draft(text string) edit {
	e := edit{op: m.op, typ: m.v.Type, key: m.v.Key, index: m.sel, text: text, keepTTL: m.v.TTL >= 0}
	if m.op == opAdd {
		e.target = m.first
		return e
	}
	if m.sel < 0 || m.sel >= m.rows() {
		return e
	}
	switch m.v.Type {
	case "hash":
		e.target, e.old = m.v.Fields[m.sel][0], m.v.Fields[m.sel][1]
	case "list":
		e.old = m.v.Items[m.sel]
	case "set":
		e.target, e.old = m.v.Members[m.sel], m.v.Members[m.sel]
	case "zset":
		e.target, e.old = m.v.ZMembers[m.sel][0], m.v.ZMembers[m.sel][1]
	}
	if m.v.Type == "string" {
		e.old = m.v.String
	}
	return e
}

// act 는 명령을 만들어 Action 으로 돌려준다. 바뀐 것이 없으면 실행하지 않는다.
func (m Model) act(e edit) Action {
	run, err := build(e)
	if err != nil {
		return Action{Status: err.Error()}
	}
	if len(run) == 0 {
		return Action{Status: "변경 없음"}
	}
	key := e.key
	switch e.op {
	case opRename:
		key = e.text
	case opDeleteKey:
		key = ""
	}
	names := make([]string, len(run))
	for i, c := range run {
		names[i] = c[0]
	}
	return Action{Run: run, Key: key, Status: strings.Join(names, "+") + " " + textutil.Printable(e.key, false)}
}

func (m Model) rows() int {
	switch m.v.Type {
	case "string":
		return len(m.strLines)
	case "hash":
		return len(m.v.Fields)
	case "list":
		return len(m.v.Items)
	case "set":
		return len(m.v.Members)
	case "zset":
		return len(m.v.ZMembers)
	}
	return 0
}

// clamp 는 커서를 항목 안으로, 스크롤을 커서가 보이게 맞춘다. string 은 커서가 곧 첫 줄이다.
func (m Model) clamp() Model {
	body := m.bodyHeight()
	m.sel = max(0, min(m.sel, m.rows()-1))
	if m.v.Type == "string" {
		m.sel = min(m.sel, max(0, m.rows()-body))
		m.top = m.sel
		return m
	}
	m.top = max(0, min(m.top, m.sel), m.sel-body+1)
	return m
}

func (m Model) bodyHeight() int { return max(1, m.h-2) }

// build 는 편집 하나를 Redis 명령 인자로 만든다. 값은 언제나 인자 하나로 넣고 명령 문자열을 이어 붙이지 않는다.
// 바뀐 것이 없으면 빈 목록이다.
func build(e edit) ([][]string, error) {
	if len(e.text) > MaxStringEdit {
		return nil, errTooLarge
	}
	switch e.op {
	case opTTL:
		secs, persist, err := parseTTL(e.text)
		if err != nil {
			return nil, err
		}
		if persist {
			return [][]string{{"PERSIST", e.key}}, nil
		}
		return [][]string{{"EXPIRE", e.key, strconv.FormatInt(secs, 10)}}, nil
	case opRename:
		if e.text == "" {
			return nil, errors.New("new key name is empty")
		}
		if e.text == e.key {
			return nil, nil
		}
		// RENAME 은 대상 키가 있으면 말없이 덮어써 남의 키를 지울 수 있다. 결과 0 이면 바꾸지 않은 것이다
		return [][]string{{"RENAMENX", e.key, e.text}}, nil
	case opDeleteKey:
		return [][]string{{"DEL", e.key}}, nil
	case opEdit:
		if e.text == e.old {
			return nil, nil
		}
		switch e.typ {
		case "string":
			// SET 은 TTL 을 지운다. KEEPTTL 은 Redis 6.0 부터라 TTL 이 있을 때만 붙인다
			if e.keepTTL {
				return [][]string{{"SET", e.key, e.text, "KEEPTTL"}}, nil
			}
			return [][]string{{"SET", e.key, e.text}}, nil
		case "hash":
			return [][]string{{"HSET", e.key, e.target, e.text}}, nil
		case "list":
			return [][]string{{"EVAL", listSetScript, "1", e.key, strconv.Itoa(e.index), e.old, e.text}}, nil
		case "set":
			// 바꾼 값이 이미 있으면 SADD 가 아무것도 하지 않고 SREM 만 되어 합쳐진다. 값을 잃지는 않는다
			return [][]string{{"SADD", e.key, e.text}, {"SREM", e.key, e.target}}, nil
		case "zset":
			score, err := parseScore(e.text)
			if err != nil {
				return nil, err
			}
			return [][]string{{"ZADD", e.key, "XX", score, e.target}}, nil
		}
	case opAdd:
		switch e.typ {
		case "hash":
			// 추가는 기존 필드를 덮어쓰지 않는다. 고치기는 e 로 한다
			return [][]string{{"HSETNX", e.key, e.target, e.text}}, nil
		case "list":
			return [][]string{{"RPUSH", e.key, e.text}}, nil
		case "set":
			return [][]string{{"SADD", e.key, e.text}}, nil
		case "zset":
			score, err := parseScore(e.text)
			if err != nil {
				return nil, err
			}
			return [][]string{{"ZADD", e.key, "NX", score, e.target}}, nil
		}
	case opDelete:
		switch e.typ {
		case "hash":
			return [][]string{{"HDEL", e.key, e.target}}, nil
		case "list":
			// 같은 값이 여럿일 때 LREM 값 1 은 고른 것이 아닌 첫 번째를 지운다. 스크립트 안에서 고른 칸을 다른 값과
			// 겹치지 않는 표식으로 바꾼 뒤 그 표식을 지워, 중간에 다른 클라이언트가 끼어들 틈이 없다
			b := make([]byte, 16)
			if _, err := rand.Read(b); err != nil {
				return nil, fmt.Errorf("make list delete marker: %w", err)
			}
			s := sentinelPrefix + hex.EncodeToString(b)
			return [][]string{{"EVAL", listDeleteScript, "1", e.key, strconv.Itoa(e.index), e.old, s}}, nil
		case "set":
			return [][]string{{"SREM", e.key, e.target}}, nil
		case "zset":
			return [][]string{{"ZREM", e.key, e.target}}, nil
		}
	}
	return nil, fmt.Errorf("%s: edit not supported", e.typ)
}

// parseTTL 은 초 단위 TTL 입력을 읽는다. 비었거나 -1 이면 만료를 없앤다(PERSIST).
// 0 이하는 EXPIRE 가 키를 바로 지우므로 받지 않는다.
func parseTTL(s string) (int64, bool, error) {
	s = strings.TrimSpace(s)
	if s == "" || s == "-1" {
		return 0, true, nil
	}
	n, err := strconv.ParseInt(s, 10, 64)
	if err != nil {
		return 0, false, fmt.Errorf("%w: %w", errTTL, err)
	}
	if n <= 0 {
		return 0, false, errTTL
	}
	return n, false, nil
}

// parseScore 는 zset 점수를 Redis 가 받는 표기로 고친다. NaN 은 Redis 가 거부하므로 미리 막는다.
func parseScore(s string) (string, error) {
	f, err := strconv.ParseFloat(strings.TrimSpace(s), 64)
	if err != nil {
		return "", fmt.Errorf("score must be a number: %w", err)
	}
	switch {
	case math.IsNaN(f):
		return "", errors.New("score must not be NaN")
	case math.IsInf(f, 1):
		return "+inf", nil
	case math.IsInf(f, -1):
		return "-inf", nil
	}
	return strconv.FormatFloat(f, 'g', -1, 64), nil
}

// update 는 입력 키를 처리한다. 버퍼는 새로 만들어 이전 Model 사본과 배열을 나누지 않는다.
func (in input) update(msg tea.KeyMsg) input {
	switch msg.Type {
	case tea.KeyRunes:
		in.buf = slices.Concat(in.buf[:in.pos], msg.Runes, in.buf[in.pos:])
		in.pos += len(msg.Runes)
	case tea.KeySpace:
		in.buf = slices.Concat(in.buf[:in.pos], []rune{' '}, in.buf[in.pos:])
		in.pos++
	case tea.KeyTab:
		in.buf = slices.Concat(in.buf[:in.pos], []rune{'\t'}, in.buf[in.pos:])
		in.pos++
	case tea.KeyBackspace:
		if in.pos > 0 {
			in.buf = slices.Concat(in.buf[:in.pos-1], in.buf[in.pos:])
			in.pos--
		}
	case tea.KeyDelete:
		if in.pos < len(in.buf) {
			in.buf = slices.Concat(in.buf[:in.pos], in.buf[in.pos+1:])
		}
	case tea.KeyCtrlU:
		in.buf = slices.Clone(in.buf[in.pos:])
		in.pos = 0
	case tea.KeyLeft:
		in.pos = max(0, in.pos-1)
	case tea.KeyRight:
		in.pos = min(len(in.buf), in.pos+1)
	case tea.KeyHome, tea.KeyCtrlA:
		in.pos = 0
	case tea.KeyEnd, tea.KeyCtrlE:
		in.pos = len(in.buf)
	}
	return in
}
