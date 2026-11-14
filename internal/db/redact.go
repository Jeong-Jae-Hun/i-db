package db

import (
	"net/url"
	"regexp"
	"strings"
)

// MaskedSQL 은 비밀값이 든 실행 글 대신 기록·화면에 남기는 글이다.
const MaskedSQL = "-- (비밀값이 있어 기록하지 않음)"

var (
	percentRe = regexp.MustCompile(`%[0-9A-Fa-f]{2}`)
	// PASSWORD 뒤 문자열(ALTER/CREATE ROLE·USER), Redis 의 AUTH·ACL SETUSER·CONFIG SET requirepass/masterauth,
	// AUTH 인자를 받는 HELLO·MIGRATE 줄이다. 리터럴만 골라 지우면 따옴표·이스케이프 변형에서 새므로 글 전체를 가린다
	secretRe = regexp.MustCompile(`(?im)\bpassword\s+(e?'|\$)|^\s*(auth|acl\s+setuser|config\s+set\s+(requirepass|masterauth))\b|^\s*(hello|migrate)\b.*\bauth2?\b`)
)

// MaskSecrets 는 비밀번호를 싣는 문장이 있으면 글 전체를 MaskedSQL 로 바꾼다. 실행 기록이 비밀값을 평문으로 남기지 않게 한다.
func MaskSecrets(text string) string {
	if secretRe.MatchString(text) {
		return MaskedSQL
	}
	return text
}

// Redact 는 서버 오류가 비밀번호를 되돌려줘도 화면·MCP 응답·기록에 나오지 않게 지운다.
// 원문과 URL 인코딩 세 가지(query·path·접속 URL 의 userinfo)를 지우고, %HH 의 16진수는 대소문자를 가리지 않는다.
func Redact(s, secret string) string {
	if secret == "" {
		return s
	}
	userinfo := strings.TrimPrefix(url.UserPassword("u", secret).String(), "u:")
	for _, v := range []string{secret, url.QueryEscape(secret), url.PathEscape(secret), userinfo} {
		var pat strings.Builder
		last := 0
		for _, loc := range percentRe.FindAllStringIndex(v, -1) {
			pat.WriteString(regexp.QuoteMeta(v[last:loc[0]]))
			pat.WriteString("%(?i:" + v[loc[0]+1:loc[1]] + ")")
			last = loc[1]
		}
		pat.WriteString(regexp.QuoteMeta(v[last:]))
		s = regexp.MustCompile(pat.String()).ReplaceAllLiteralString(s, "***")
	}
	return s
}
