package awsres

import (
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
)

// 이 패키지는 AWS 에 읽기만 보낸다(R67.1). 소스에서 쓰기 작업 호출을 찾으면 실패하고, 서비스 클라이언트 필드로 부르는
// 메서드는 List·Get·Describe·Head 로 시작해야 한다. 페이지네이터(sns.NewList…Paginator)도 이름이 List 라 같은 규칙이다.
var (
	writeCall  = regexp.MustCompile(`\.(ReceiveMessage|SendMessage\w*|Publish\w*|Purge\w*|Delete\w*|Put\w*|Start\w*|Create\w*|Update\w*|Invoke\w*|Tag\w*|Untag\w*|Copy\w*|Restore\w*|Stop\w*|Set(Queue|Subscription|Topic)\w*|Add\w*|Remove\w*|Subscribe|Unsubscribe|ChangeMessage\w*|Redrive\w*|Enable\w*|Disable\w*|Send\w*)\(`)
	clientCall = regexp.MustCompile(`b\.(sts|sns|sqs|lambda|events|scheduler|sfn|s3)\.(\w+)\(`)
	paginator  = regexp.MustCompile(`\.New(\w+)Paginator\(`)
	readPrefix = regexp.MustCompile(`^(List|Get|Describe|Head)`)
)

func TestReadOnly_패키지는_쓰기_API_를_부르지_않는다(t *testing.T) {
	files, err := filepath.Glob("*.go")
	if err != nil {
		t.Fatal(err)
	}
	checked := 0
	for _, f := range files {
		if strings.HasSuffix(f, "_test.go") {
			continue
		}
		src, err := os.ReadFile(f)
		if err != nil {
			t.Fatal(err)
		}
		checked++
		for i, line := range strings.Split(string(src), "\n") {
			code, _, _ := strings.Cut(line, "//")
			if m := writeCall.FindString(code); m != "" {
				t.Errorf("%s:%d: 쓰기 작업 호출 %s", f, i+1, m)
			}
			for _, m := range clientCall.FindAllStringSubmatch(code, -1) {
				if !readPrefix.MatchString(m[2]) {
					t.Errorf("%s:%d: 읽기가 아닌 클라이언트 호출 %s.%s", f, i+1, m[1], m[2])
				}
			}
			for _, m := range paginator.FindAllStringSubmatch(code, -1) {
				if !readPrefix.MatchString(m[1]) {
					t.Errorf("%s:%d: 읽기가 아닌 페이지네이터 %s", f, i+1, m[1])
				}
			}
		}
	}
	if checked == 0 {
		t.Fatal("소스 파일을 하나도 읽지 않았다")
	}
}

func TestReadOnly_검사는_쓰기_호출을_실제로_잡는다(t *testing.T) {
	for _, line := range []string{`b.sqs.SendMessage(ctx, in)`, `b.sns.Publish(ctx, in)`, `x.PurgeQueue(ctx, in)`, `b.s3.PutObject(ctx, in)`, `b.sfn.StartExecution(ctx, in)`, `cl.DeleteMessage(ctx, in)`} {
		if writeCall.FindString(line) == "" {
			t.Errorf("못 잡음: %s", line)
		}
	}
	if m := clientCall.FindStringSubmatch(`b.sqs.ChangeMessageVisibility(ctx, in)`); m == nil || readPrefix.MatchString(m[2]) {
		t.Errorf("클라이언트 호출 접두 검사: %v", m)
	}
	for _, line := range []string{`b.sqs.GetQueueAttributes(ctx, in)`, `sqs.NewListQueuesPaginator(b.sqs, in)`, `time.Since(start)`, `n.StartTime`} {
		if m := writeCall.FindString(line); m != "" {
			t.Errorf("읽기를 쓰기로 봄: %s → %s", line, m)
		}
	}
}

// 대시보드 확인(probe.go)과 흐름 풀이(flow.go)는 요금 허용(R72)과 상관없이 늘 돈다. 그래서 요청마다 요금이 붙는
// SNS·SQS·S3 클라이언트를 소스에서 아예 쓰지 않아야 한다. 덫 가짜로 본 시험(Probe·QueueFlows)과 함께 새 호출이 끼는 것을 막는다.
var billedClient = regexp.MustCompile(`\bb\.(sns|sqs|s3)\b|\b(sns|sqs|s3)\.New\w*Paginator\(`)

func TestReadOnly_늘_도는_대시보드와_흐름은_요금_서비스_클라이언트를_쓰지_않는다(t *testing.T) {
	for _, f := range []string{"probe.go", "flow.go"} {
		src, err := os.ReadFile(f)
		if err != nil {
			t.Fatal(err)
		}
		for i, line := range strings.Split(string(src), "\n") {
			code, _, _ := strings.Cut(line, "//")
			if m := billedClient.FindString(code); m != "" {
				t.Errorf("%s:%d: 요금 서비스 호출 %s", f, i+1, m)
			}
		}
	}
	for _, line := range []string{`b.sqs.GetQueueAttributes(ctx, in)`, `sns.NewListTopicsPaginator(b.sns, in)`, `p := s3.NewListObjectsV2Paginator(c, in)`} {
		if billedClient.FindString(line) == "" {
			t.Errorf("못 잡음: %s", line)
		}
	}
}
