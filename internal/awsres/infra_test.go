package awsres

import (
	"context"
	"io"
	"reflect"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/eventbridge"
	ebtypes "github.com/aws/aws-sdk-go-v2/service/eventbridge/types"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	s3types "github.com/aws/aws-sdk-go-v2/service/s3/types"
	"github.com/aws/aws-sdk-go-v2/service/scheduler"
	schedtypes "github.com/aws/aws-sdk-go-v2/service/scheduler/types"
	"github.com/aws/aws-sdk-go-v2/service/sfn"
	sfntypes "github.com/aws/aws-sdk-go-v2/service/sfn/types"
)

// ─── 가짜 EventBridge·Scheduler·Step Functions·S3 ───

type fakeEvents struct {
	buses   [][]string // 페이지마다 버스 이름
	rules   map[string][]ebtypes.Rule
	targets map[string][]ebtypes.Target // "<버스>|<규칙>" → 대상
	asked   []string
}

func (f *fakeEvents) ListEventBuses(_ context.Context, in *eventbridge.ListEventBusesInput, _ ...func(*eventbridge.Options)) (*eventbridge.ListEventBusesOutput, error) {
	i := 0
	if in.NextToken != nil {
		i = int((*in.NextToken)[0] - '0')
	}
	out := &eventbridge.ListEventBusesOutput{}
	for _, n := range f.buses[i] {
		out.EventBuses = append(out.EventBuses, ebtypes.EventBus{Name: aws.String(n)})
	}
	if i+1 < len(f.buses) {
		out.NextToken = aws.String(string(rune('0' + i + 1)))
	}
	return out, nil
}

func (f *fakeEvents) ListRules(_ context.Context, in *eventbridge.ListRulesInput, _ ...func(*eventbridge.Options)) (*eventbridge.ListRulesOutput, error) {
	return &eventbridge.ListRulesOutput{Rules: f.rules[aws.ToString(in.EventBusName)]}, nil
}

func (f *fakeEvents) ListTargetsByRule(_ context.Context, in *eventbridge.ListTargetsByRuleInput, _ ...func(*eventbridge.Options)) (*eventbridge.ListTargetsByRuleOutput, error) {
	k := aws.ToString(in.EventBusName) + "|" + aws.ToString(in.Rule)
	f.asked = append(f.asked, k)
	return &eventbridge.ListTargetsByRuleOutput{Targets: f.targets[k]}, nil
}

type fakeScheduler struct {
	groups    []string
	schedules map[string][]string
	detail    map[string]*scheduler.GetScheduleOutput // "<그룹>/<이름>"
}

func (f *fakeScheduler) ListScheduleGroups(context.Context, *scheduler.ListScheduleGroupsInput, ...func(*scheduler.Options)) (*scheduler.ListScheduleGroupsOutput, error) {
	out := &scheduler.ListScheduleGroupsOutput{}
	for _, g := range f.groups {
		out.ScheduleGroups = append(out.ScheduleGroups, schedtypes.ScheduleGroupSummary{Name: aws.String(g)})
	}
	return out, nil
}

func (f *fakeScheduler) ListSchedules(_ context.Context, in *scheduler.ListSchedulesInput, _ ...func(*scheduler.Options)) (*scheduler.ListSchedulesOutput, error) {
	out := &scheduler.ListSchedulesOutput{}
	for _, n := range f.schedules[aws.ToString(in.GroupName)] {
		out.Schedules = append(out.Schedules, schedtypes.ScheduleSummary{Name: aws.String(n)})
	}
	return out, nil
}

func (f *fakeScheduler) GetSchedule(_ context.Context, in *scheduler.GetScheduleInput, _ ...func(*scheduler.Options)) (*scheduler.GetScheduleOutput, error) {
	d, ok := f.detail[aws.ToString(in.GroupName)+"/"+aws.ToString(in.Name)]
	if !ok {
		return nil, denied("AccessDeniedException")
	}
	return d, nil
}

type fakeSFN struct {
	machines []sfntypes.StateMachineListItem
	execs    []sfntypes.ExecutionListItem
	next     string
	limit    int32
}

func (f *fakeSFN) ListStateMachines(context.Context, *sfn.ListStateMachinesInput, ...func(*sfn.Options)) (*sfn.ListStateMachinesOutput, error) {
	return &sfn.ListStateMachinesOutput{StateMachines: f.machines}, nil
}

func (f *fakeSFN) ListExecutions(_ context.Context, in *sfn.ListExecutionsInput, _ ...func(*sfn.Options)) (*sfn.ListExecutionsOutput, error) {
	f.limit = in.MaxResults
	out := &sfn.ListExecutionsOutput{Executions: f.execs}
	if f.next != "" {
		out.NextToken = aws.String(f.next)
	}
	return out, nil
}

type fakeS3 struct {
	mu        sync.Mutex
	buckets   []s3types.Bucket
	listing   map[string]*s3.ListObjectsV2Output // "<버킷>|<접두>"
	location  map[string]s3types.BucketLocationConstraint
	heads     map[string]*s3.HeadObjectOutput // 키
	bodies    map[string]string
	regions   []string // 요청마다 보낸 리전(옵션 적용 뒤)
	ranges    []string
	locations int
}

func (f *fakeS3) region(opts []func(*s3.Options)) {
	o := s3.Options{Region: "ap-northeast-2"}
	for _, fn := range opts {
		fn(&o)
	}
	f.regions = append(f.regions, o.Region)
}

func (f *fakeS3) ListBuckets(context.Context, *s3.ListBucketsInput, ...func(*s3.Options)) (*s3.ListBucketsOutput, error) {
	return &s3.ListBucketsOutput{Buckets: f.buckets}, nil
}

func (f *fakeS3) ListObjectsV2(_ context.Context, in *s3.ListObjectsV2Input, opts ...func(*s3.Options)) (*s3.ListObjectsV2Output, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.region(opts)
	if aws.ToString(in.Delimiter) != "/" || aws.ToInt32(in.MaxKeys) != s3Level {
		return nil, denied("InvalidRequest")
	}
	return f.listing[aws.ToString(in.Bucket)+"|"+aws.ToString(in.Prefix)], nil
}

func (f *fakeS3) GetBucketLocation(_ context.Context, in *s3.GetBucketLocationInput, _ ...func(*s3.Options)) (*s3.GetBucketLocationOutput, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.locations++
	return &s3.GetBucketLocationOutput{LocationConstraint: f.location[aws.ToString(in.Bucket)]}, nil
}

func (f *fakeS3) HeadObject(_ context.Context, in *s3.HeadObjectInput, opts ...func(*s3.Options)) (*s3.HeadObjectOutput, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.region(opts)
	return f.heads[aws.ToString(in.Key)], nil
}

func (f *fakeS3) GetObject(_ context.Context, in *s3.GetObjectInput, opts ...func(*s3.Options)) (*s3.GetObjectOutput, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.region(opts)
	f.ranges = append(f.ranges, aws.ToString(in.Range))
	return &s3.GetObjectOutput{Body: io.NopCloser(strings.NewReader(f.bodies[aws.ToString(in.Key)]))}, nil
}

// ─── EventBridge ───

func TestEventBridge_버스_규칙_대상을_보이고_파트너_버스의_슬래시를_견딘다(t *testing.T) {
	b, _, _, _ := newBackend()
	partner := "aws.partner/saas.com/123/feed"
	ev := &fakeEvents{
		buses: [][]string{{"default"}, {partner}},
		rules: map[string][]ebtypes.Rule{
			"default": {
				{Name: aws.String("nightly"), ScheduleExpression: aws.String("cron(0 18 * * ? *)"), State: ebtypes.RuleStateEnabled},
				{Name: aws.String("on-order"), EventPattern: aws.String(`{"source":["app"]}`), State: ebtypes.RuleStateDisabled},
			},
			partner: {{Name: aws.String("p"), EventPattern: aws.String("{}"), State: ebtypes.RuleStateEnabled}},
		},
		targets: map[string][]ebtypes.Target{
			"default|on-order": {
				{Id: aws.String("1"), Arn: aws.String("arn:aws:sqs:ap-northeast-2:111122223333:orders"), Input: aws.String(`{"a":1}`),
					DeadLetterConfig: &ebtypes.DeadLetterConfig{Arn: aws.String("arn:aws:sqs:ap-northeast-2:111122223333:eb-dlq")}},
				{Id: aws.String("2"), Arn: aws.String("arn:aws:lambda:ap-northeast-2:111122223333:function:w"), InputPath: aws.String("$.detail")},
				{Id: aws.String("3"), Arn: aws.String("arn:aws:sns:ap-northeast-2:111122223333:t"), InputTransformer: &ebtypes.InputTransformer{InputTemplate: aws.String(`"<id>"`)}},
			},
			partner + "|p": {{Id: aws.String("x"), Arn: aws.String("arn:x")}},
		},
	}
	b.events = ev
	buses, err := b.Children(ctx, "events")
	if err != nil || !reflect.DeepEqual(names(buses), []string{"default", partner}) || !buses[0].HasChildren {
		t.Fatalf("버스: %v %v", buses, err)
	}
	rules, err := b.Children(ctx, buses[0].Path)
	if err != nil || rules[0].Info != "cron(0 18 * * ? *) · ENABLED" || rules[1].Info != "pattern · DISABLED" || !rules[1].Openable || rules[1].HasChildren {
		t.Fatalf("규칙: %+v %v", rules, err)
	}
	res, err := b.Open(ctx, rules[1].Path, 50)
	if err != nil {
		t.Fatal(err)
	}
	want := [][]string{
		{"1", "arn:aws:sqs:ap-northeast-2:111122223333:orders", `{"a":1}`, "arn:aws:sqs:ap-northeast-2:111122223333:eb-dlq"},
		{"2", "arn:aws:lambda:ap-northeast-2:111122223333:function:w", "$.detail", ""},
		{"3", "arn:aws:sns:ap-northeast-2:111122223333:t", `"<id>"`, ""},
	}
	if !reflect.DeepEqual(res.Cols, []string{"target_id", "arn", "input", "dlq"}) || !reflect.DeepEqual(res.Rows, want) {
		t.Fatalf("%v %v", res.Cols, res.Rows)
	}
	prules, _ := b.Children(ctx, buses[1].Path)
	if res, err := b.Open(ctx, prules[0].Path, 50); err != nil || len(res.Rows) != 1 || ev.asked[len(ev.asked)-1] != partner+"|p" {
		t.Fatalf("파트너 버스: %v %v %v", res, err, ev.asked)
	}
}

// ─── Scheduler ───

func TestScheduler_그룹_일정에_식과_상태를_보이고_Enter_는_대상이다(t *testing.T) {
	b, _, _, _ := newBackend()
	b.scheduler = &fakeScheduler{
		groups:    []string{"default"},
		schedules: map[string][]string{"default": {"daily", "secret"}},
		detail: map[string]*scheduler.GetScheduleOutput{"default/daily": {
			ScheduleExpression: aws.String("rate(1 day)"), ScheduleExpressionTimezone: aws.String("Asia/Seoul"), State: schedtypes.ScheduleStateEnabled,
			Target: &schedtypes.Target{Arn: aws.String("arn:aws:lambda:ap-northeast-2:111122223333:function:w"), Input: aws.String("{}"), RoleArn: aws.String("arn:role")},
		}},
	}
	groups, err := b.Children(ctx, "scheduler")
	if err != nil || !reflect.DeepEqual(names(groups), []string{"default"}) || !groups[0].HasChildren {
		t.Fatalf("그룹: %v %v", groups, err)
	}
	ss, err := b.Children(ctx, groups[0].Path)
	if err != nil || ss[0].Info != "rate(1 day) · ENABLED" || ss[1].Info != "권한 없음: scheduler:GetSchedule" || ss[0].Path != "scheduler/default/daily" {
		t.Fatalf("일정: %+v %v", ss, err)
	}
	res, err := b.Open(ctx, ss[0].Path, 50)
	want := [][]string{{"expression", "rate(1 day)"}, {"timezone", "Asia/Seoul"}, {"state", "ENABLED"},
		{"target_arn", "arn:aws:lambda:ap-northeast-2:111122223333:function:w"}, {"input", "{}"}, {"role_arn", "arn:role"}}
	if err != nil || !reflect.DeepEqual(res.Rows, want) {
		t.Fatalf("%v %v", res, err)
	}
}

// ─── Step Functions ───

func TestStepFunctions_상태_기계_Enter_는_최근_50개_실행이고_더_있으면_잘림이다(t *testing.T) {
	b, _, _, _ := newBackend()
	start := time.Date(2026, 11, 16, 1, 2, 3, 0, time.UTC)
	f := &fakeSFN{
		machines: []sfntypes.StateMachineListItem{{Name: aws.String("order-flow"), StateMachineArn: aws.String("arn:aws:states:ap-northeast-2:111122223333:stateMachine:order-flow"),
			Type: sfntypes.StateMachineTypeStandard, CreationDate: aws.Time(start)}},
		execs: []sfntypes.ExecutionListItem{
			{Name: aws.String("run-2"), Status: sfntypes.ExecutionStatusRunning, StartDate: aws.Time(start)},
			{Name: aws.String("run-1"), Status: sfntypes.ExecutionStatusFailed, StartDate: aws.Time(start.Add(-time.Hour)), StopDate: aws.Time(start.Add(-59 * time.Minute))},
		},
		next: "more",
	}
	b.sfn = f
	ms, err := b.Children(ctx, "sfn")
	if err != nil || len(ms) != 1 || ms[0].Info != "STANDARD · 2026-11-16 10:02" || !ms[0].Openable {
		t.Fatalf("상태 기계: %+v %v", ms, err)
	}
	res, err := b.Open(ctx, ms[0].Path, 50)
	if err != nil {
		t.Fatal(err)
	}
	want := [][]string{{"run-2", "RUNNING", "2026-11-16 10:02:03", ""}, {"run-1", "FAILED", "2026-11-16 09:02:03", "2026-11-16 09:03:03"}}
	if !reflect.DeepEqual(res.Cols, []string{"name", "status", "start", "stop"}) || !reflect.DeepEqual(res.Rows, want) || !res.Truncated || f.limit != 50 {
		t.Fatalf("%v %v truncated=%v limit=%d", res.Cols, res.Rows, res.Truncated, f.limit)
	}
}

// ─── S3 ───

func s3Fixture() *fakeS3 {
	mod := aws.Time(time.Date(2026, 11, 1, 0, 0, 0, 0, time.UTC))
	return &fakeS3{
		buckets: []s3types.Bucket{{Name: aws.String("logs"), BucketRegion: aws.String("us-west-2")}, {Name: aws.String("old")}},
		listing: map[string]*s3.ListObjectsV2Output{
			"logs|": {CommonPrefixes: []s3types.CommonPrefix{{Prefix: aws.String("2026/")}}, Contents: []s3types.Object{{Key: aws.String("README.txt"), Size: aws.Int64(1536), LastModified: mod}}},
			"logs|2026/": {Contents: []s3types.Object{{Key: aws.String("2026/")}, {Key: aws.String("2026/a.json"), Size: aws.Int64(10), LastModified: mod}},
				IsTruncated: aws.Bool(true)},
			"old|": {},
		},
		location: map[string]s3types.BucketLocationConstraint{"old": "EU"},
		heads: map[string]*s3.HeadObjectOutput{
			"2026/a.json": {ContentLength: aws.Int64(10), ContentType: aws.String("application/json"), ETag: aws.String(`"e"`), LastModified: mod,
				Metadata: map[string]string{"owner": "ops"}},
			"big.log": {ContentLength: aws.Int64(s3Peek + 1), ContentType: aws.String("text/plain")},
			"img.png": {ContentLength: aws.Int64(10), ContentType: aws.String("image/png")},
			"bin.txt": {ContentLength: aws.Int64(3)},
			"gz.json": {ContentLength: aws.Int64(3), ContentType: aws.String("application/json"), ContentEncoding: aws.String("gzip")},
		},
		bodies: map[string]string{"2026/a.json": `{"ok":true}`, "bin.txt": "\xff\xfe\x00"},
	}
}

func TestS3_버킷_접두_객체를_한_단계씩_보이고_버킷_리전으로_묻는다(t *testing.T) {
	b, _, _, _ := newBackend()
	f := s3Fixture()
	b.s3 = f
	bs, err := b.Children(ctx, "s3")
	if err != nil || !reflect.DeepEqual(names(bs), []string{"logs", "old"}) || bs[0].Info != "us-west-2" || !bs[0].HasChildren {
		t.Fatalf("버킷: %+v %v", bs, err)
	}
	top, err := b.Children(ctx, bs[0].Path)
	if err != nil || !reflect.DeepEqual(names(top), []string{"2026/", "README.txt"}) {
		t.Fatalf("맨 위: %+v %v", top, err)
	}
	if top[0].Path != "s3/logs/2026/" || !top[0].HasChildren || top[1].Info != "1.5KB · 2026-11-01 09:00" || !top[1].Openable {
		t.Fatalf("노드: %+v", top)
	}
	sub, err := b.Children(ctx, top[0].Path)
	if err != nil || !reflect.DeepEqual(names(sub), []string{"a.json", "… 앞 1000개만 보임"}) || sub[1].Openable || sub[1].HasChildren {
		t.Fatalf("접두 아래(빈 폴더 객체는 뺌): %+v %v", sub, err)
	}
	if leaf, _ := b.Children(ctx, sub[0].Path); leaf != nil {
		t.Fatal("객체는 잎이다")
	}
	if _, err := b.Children(ctx, bs[1].Path); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(f.regions, []string{"us-west-2", "us-west-2", "eu-west-1"}) || f.locations != 1 {
		t.Fatalf("리전: %v, GetBucketLocation %d번", f.regions, f.locations)
	}
}

func TestS3_객체_Enter_는_메타데이터이고_64KiB_이하_텍스트만_Range_로_본문을_붙인다(t *testing.T) {
	b, _, _, _ := newBackend()
	f := s3Fixture()
	b.s3 = f
	b.regions.Store("logs", "us-west-2")
	res, err := b.Open(ctx, "s3/logs/2026/a.json", 50)
	if err != nil {
		t.Fatal(err)
	}
	got := map[string]string{}
	for _, r := range res.Rows {
		got[r[0]] = r[1]
	}
	if got["key"] != "2026/a.json" || got["size"] != "10" || got["content_type"] != "application/json" || got["x-amz-meta-owner"] != "ops" ||
		got["body"] != `{"ok":true}` || got["last_modified"] != "2026-11-01 09:00:00" {
		t.Fatalf("표: %v", res.Rows)
	}
	if !reflect.DeepEqual(f.ranges, []string{"bytes=0-65535"}) {
		t.Fatalf("본문은 Range 로만: %v", f.ranges)
	}
	for key, body := range map[string]string{"big.log": "(64KiB 를 넘어 받지 않음)", "img.png": "", "gz.json": "", "bin.txt": "(텍스트가 아님)"} {
		res, err := b.Open(ctx, "s3/logs/"+key, 50)
		if err != nil {
			t.Fatal(err)
		}
		last := res.Rows[len(res.Rows)-1]
		if gotBody := map[bool]string{true: last[1], false: ""}[last[0] == "body"]; gotBody != body {
			t.Errorf("%s: body %q, want %q", key, gotBody, body)
		}
	}
	if len(f.ranges) != 2 {
		t.Fatalf("큰 객체·이미지·압축 객체는 GetObject 를 부르지 않는다: %v", f.ranges)
	}
	if _, err := b.Open(ctx, "s3/logs/2026/", 50); err == nil {
		t.Fatal("접두는 열 것이 없다")
	}
}

func TestTextLike_Content_Type_과_확장자로_가른다(t *testing.T) {
	cases := map[[2]string]bool{
		{"text/plain; charset=utf-8", "a.bin"}: true,
		{"application/json", "a"}:              true,
		{"application/vnd.api+json", "a"}:      true,
		{"", "a.CSV"}:                          true,
		{"application/octet-stream", "a.log"}:  true,
		{"application/octet-stream", "a.gz"}:   false,
		{"image/png", "a.txt"}:                 false,
	}
	for c, want := range cases {
		if got := textLike(c[0], c[1]); got != want {
			t.Errorf("%v: %v", c, got)
		}
	}
}
