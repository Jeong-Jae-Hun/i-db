package awsres

import (
	"context"
	"fmt"
	"strings"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/eventbridge"
	"github.com/aws/aws-sdk-go-v2/service/scheduler"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── EventBridge (R67.4) ───

type eventsAPI interface {
	ListEventBuses(context.Context, *eventbridge.ListEventBusesInput, ...func(*eventbridge.Options)) (*eventbridge.ListEventBusesOutput, error)
	ListRules(context.Context, *eventbridge.ListRulesInput, ...func(*eventbridge.Options)) (*eventbridge.ListRulesOutput, error)
	ListTargetsByRule(context.Context, *eventbridge.ListTargetsByRuleInput, ...func(*eventbridge.Options)) (*eventbridge.ListTargetsByRuleOutput, error)
}

// eventsChildren 은 rest "" 면 버스, "<버스>" 면 규칙이다. 규칙은 잎이고 경로는 events/<버스>/<규칙>이다.
// 파트너 버스 이름(aws.partner/…)에는 / 가 있지만 규칙 이름에는 없어 Open 은 마지막 / 로 가른다.
// EventBridge SDK 에는 페이지네이터가 없어 NextToken 을 손으로 잇는다.
func (b *Backend) eventsChildren(ctx context.Context, rest string) ([]source.Node, error) {
	var out []source.Node
	var next *string
	for {
		if rest == "" {
			page, err := b.events.ListEventBuses(ctx, &eventbridge.ListEventBusesInput{NextToken: next})
			if err != nil {
				return nil, wrap("events:ListEventBuses", err)
			}
			for _, e := range page.EventBuses {
				name := aws.ToString(e.Name)
				out = append(out, source.Node{Name: name, Path: "events/" + name, HasChildren: true})
			}
			next = page.NextToken
		} else {
			page, err := b.events.ListRules(ctx, &eventbridge.ListRulesInput{EventBusName: aws.String(rest), NextToken: next})
			if err != nil {
				return nil, wrap("events:ListRules", err)
			}
			for _, r := range page.Rules {
				kind := aws.ToString(r.ScheduleExpression)
				if kind == "" {
					kind = "pattern"
				}
				name := aws.ToString(r.Name)
				out = append(out, source.Node{Name: name, Path: "events/" + rest + "/" + name, Info: kind + " · " + string(r.State), Openable: true})
			}
			next = page.NextToken
		}
		if aws.ToString(next) == "" {
			return out, nil
		}
	}
}

// targetTable 은 규칙 Enter 의 대상 표다. input 은 Input, 없으면 InputPath, 없으면 InputTransformer 의 템플릿이다.
func (b *Backend) targetTable(ctx context.Context, rest string) (*db.Result, error) {
	i := strings.LastIndex(rest, "/")
	if i < 0 {
		return nil, fmt.Errorf("nothing to open: events/%s", rest)
	}
	bus, rule := rest[:i], rest[i+1:]
	res := table("target_id", "arn", "input", "dlq")
	var next *string
	for {
		page, err := b.events.ListTargetsByRule(ctx, &eventbridge.ListTargetsByRuleInput{EventBusName: aws.String(bus), Rule: aws.String(rule), NextToken: next})
		if err != nil {
			return nil, wrap("events:ListTargetsByRule", err)
		}
		for _, t := range page.Targets {
			input := aws.ToString(t.Input)
			switch {
			case input != "":
			case t.InputPath != nil:
				input = aws.ToString(t.InputPath)
			case t.InputTransformer != nil:
				input = aws.ToString(t.InputTransformer.InputTemplate)
			}
			dlq := ""
			if t.DeadLetterConfig != nil {
				dlq = aws.ToString(t.DeadLetterConfig.Arn)
			}
			res.Rows = append(res.Rows, []string{aws.ToString(t.Id), aws.ToString(t.Arn), input, dlq})
		}
		if next = page.NextToken; aws.ToString(next) == "" {
			return res, nil
		}
	}
}

// ─── EventBridge Scheduler (R67.4) ───

type schedulerAPI interface {
	scheduler.ListScheduleGroupsAPIClient
	scheduler.ListSchedulesAPIClient
	GetSchedule(context.Context, *scheduler.GetScheduleInput, ...func(*scheduler.Options)) (*scheduler.GetScheduleOutput, error)
}

// schedulerChildren 은 rest "" 면 그룹, "<그룹>" 이면 일정이다. 목록에는 식이 없어 일정마다 GetSchedule 을 parallel 개씩 부른다.
func (b *Backend) schedulerChildren(ctx context.Context, rest string) ([]source.Node, error) {
	if rest == "" {
		var out []source.Node
		p := scheduler.NewListScheduleGroupsPaginator(b.scheduler, &scheduler.ListScheduleGroupsInput{})
		for p.HasMorePages() {
			page, err := p.NextPage(ctx)
			if err != nil {
				return nil, wrap("scheduler:ListScheduleGroups", err)
			}
			for _, g := range page.ScheduleGroups {
				name := aws.ToString(g.Name)
				out = append(out, source.Node{Name: name, Path: "scheduler/" + name, HasChildren: true})
			}
		}
		return out, nil
	}
	if strings.Contains(rest, "/") {
		return nil, nil
	}
	var names []string
	p := scheduler.NewListSchedulesPaginator(b.scheduler, &scheduler.ListSchedulesInput{GroupName: aws.String(rest)})
	for p.HasMorePages() {
		page, err := p.NextPage(ctx)
		if err != nil {
			return nil, wrap("scheduler:ListSchedules", err)
		}
		for _, s := range page.Schedules {
			names = append(names, aws.ToString(s.Name))
		}
	}
	out := make([]source.Node, len(names))
	each(len(names), func(i int) {
		n := source.Node{Name: names[i], Path: "scheduler/" + rest + "/" + names[i], Openable: true}
		s, err := b.schedule(ctx, rest, names[i])
		switch {
		case deniedAction(err) != "":
			n.Info = "권한 없음: " + deniedAction(err)
		case err != nil:
			n.Info = "읽기 실패"
		default:
			n.Info = aws.ToString(s.ScheduleExpression) + " · " + string(s.State)
		}
		out[i] = n
	})
	return out, nil
}

func (b *Backend) schedule(ctx context.Context, group, name string) (*scheduler.GetScheduleOutput, error) {
	s, err := b.scheduler.GetSchedule(ctx, &scheduler.GetScheduleInput{GroupName: aws.String(group), Name: aws.String(name)})
	return s, wrap("scheduler:GetSchedule", err)
}

// scheduleTable 은 일정 Enter 의 `attribute, value` 표다(식·시간대·상태·대상).
func (b *Backend) scheduleTable(ctx context.Context, rest string) (*db.Result, error) {
	group, name, _ := strings.Cut(rest, "/")
	s, err := b.schedule(ctx, group, name)
	if err != nil {
		return nil, err
	}
	res := table("attribute", "value")
	res.Rows = append(res.Rows, []string{"expression", aws.ToString(s.ScheduleExpression)},
		[]string{"timezone", aws.ToString(s.ScheduleExpressionTimezone)}, []string{"state", string(s.State)})
	if t := s.Target; t != nil {
		res.Rows = append(res.Rows, []string{"target_arn", aws.ToString(t.Arn)}, []string{"input", aws.ToString(t.Input)},
			[]string{"role_arn", aws.ToString(t.RoleArn)})
		if t.DeadLetterConfig != nil {
			res.Rows = append(res.Rows, []string{"dlq", aws.ToString(t.DeadLetterConfig.Arn)})
		}
	}
	return res, nil
}
