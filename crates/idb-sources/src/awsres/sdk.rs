//! aws 트리의 SDK 호출이다. 여기서 부르는 작업은 모두 List*·Get*·Head* 여야 한다(시험이 이 파일을 읽어 지킨다, R67.1).

use std::collections::HashMap;

use aws_config::{Region, SdkConfig};
use chrono::{DateTime, Utc};

use super::{
    Api, Execution, Function, Head, Listing, Machine, Object, Page, Rule, Schedule, ScheduleTarget, SourceMapping, Subscription, Target,
};
use crate::aws::{AwsErr, sdk_err};

pub struct Sdk {
    sts: aws_sdk_sts::Client,
    sns: aws_sdk_sns::Client,
    sqs: aws_sdk_sqs::Client,
    lambda: aws_sdk_lambda::Client,
    events: aws_sdk_eventbridge::Client,
    scheduler: aws_sdk_scheduler::Client,
    sfn: aws_sdk_sfn::Client,
    s3: aws_sdk_s3::Client,
}

impl Sdk {
    pub fn new(cfg: &SdkConfig) -> Sdk {
        Sdk {
            sts: aws_sdk_sts::Client::new(cfg),
            sns: aws_sdk_sns::Client::new(cfg),
            sqs: aws_sdk_sqs::Client::new(cfg),
            lambda: aws_sdk_lambda::Client::new(cfg),
            events: aws_sdk_eventbridge::Client::new(cfg),
            scheduler: aws_sdk_scheduler::Client::new(cfg),
            sfn: aws_sdk_sfn::Client::new(cfg),
            s3: aws_sdk_s3::Client::new(cfg),
        }
    }
}

fn time(t: Option<&aws_smithy_types::DateTime>) -> Option<DateTime<Utc>> {
    t.and_then(|t| DateTime::from_timestamp(t.secs(), t.subsec_nanos()))
}

fn s(v: Option<&str>) -> String {
    v.unwrap_or_default().to_string()
}

/// 버킷 리전으로 보내는 설정 덮어쓰기다. None 이면 접속 리전 그대로다.
fn s3_region(r: Option<&str>) -> aws_sdk_s3::config::Builder {
    let mut b = aws_sdk_s3::config::Builder::default();
    if let Some(r) = r {
        b = b.region(Region::new(r.to_string()));
    }
    b
}

impl Api for Sdk {
    async fn sts_caller(&self) -> Result<(String, String), AwsErr> {
        let out = self.sts.get_caller_identity().send().await.map_err(sdk_err)?;
        Ok((s(out.account()), s(out.arn())))
    }

    async fn sns_topics(&self, next: Option<String>) -> Page<String> {
        let out = self.sns.list_topics().set_next_token(next).send().await.map_err(sdk_err)?;
        Ok((out.topics().iter().map(|t| s(t.topic_arn())).collect(), out.next_token))
    }

    async fn sns_subscriptions(&self, topic: &str, next: Option<String>) -> Page<Subscription> {
        let out = self
            .sns
            .list_subscriptions_by_topic()
            .topic_arn(topic)
            .set_next_token(next)
            .send()
            .await
            .map_err(sdk_err)?;
        let subs = out
            .subscriptions()
            .iter()
            .map(|x| Subscription {
                protocol: s(x.protocol()),
                endpoint: s(x.endpoint()),
                arn: s(x.subscription_arn()),
            })
            .collect();
        Ok((subs, out.next_token))
    }

    async fn sns_subscription_attrs(&self, arn: &str) -> Result<HashMap<String, String>, AwsErr> {
        let out = self
            .sns
            .get_subscription_attributes()
            .subscription_arn(arn)
            .send()
            .await
            .map_err(sdk_err)?;
        Ok(out.attributes.unwrap_or_default())
    }

    async fn sqs_queues(&self, next: Option<String>) -> Page<String> {
        // MaxResults 를 주지 않으면 ListQueues 는 1,000개에서 다음 토큰 없이 끊는다
        let out = self
            .sqs
            .list_queues()
            .max_results(1000)
            .set_next_token(next)
            .send()
            .await
            .map_err(sdk_err)?;
        Ok((out.queue_urls.unwrap_or_default(), out.next_token))
    }

    async fn sqs_queue_attrs(&self, url: &str, names: &[&str]) -> Result<HashMap<String, String>, AwsErr> {
        let mut req = self.sqs.get_queue_attributes().queue_url(url);
        for n in names {
            req = req.attribute_names(aws_sdk_sqs::types::QueueAttributeName::from(*n));
        }
        let out = req.send().await.map_err(sdk_err)?;
        Ok(out
            .attributes
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k.as_str().to_string(), v))
            .collect())
    }

    async fn sqs_queue_url(&self, name: &str, owner: Option<&str>) -> Result<String, AwsErr> {
        let out = self
            .sqs
            .get_queue_url()
            .queue_name(name)
            .set_queue_owner_aws_account_id(owner.map(String::from))
            .send()
            .await
            .map_err(sdk_err)?;
        Ok(s(out.queue_url()))
    }

    async fn lambda_functions(&self, next: Option<String>) -> Page<Function> {
        let out = self.lambda.list_functions().set_marker(next).send().await.map_err(sdk_err)?;
        let fns = out
            .functions()
            .iter()
            .map(|f| Function {
                name: s(f.function_name()),
                runtime: f.runtime().map(|r| r.as_str().to_string()).unwrap_or_default(),
                package_type: f.package_type().map(|p| p.as_str().to_string()).unwrap_or_default(),
                last_modified: s(f.last_modified()),
            })
            .collect();
        Ok((fns, out.next_marker))
    }

    async fn lambda_mappings(&self, function: Option<&str>, source: Option<&str>, next: Option<String>) -> Page<SourceMapping> {
        let out = self
            .lambda
            .list_event_source_mappings()
            .set_function_name(function.map(String::from))
            .set_event_source_arn(source.map(String::from))
            .set_marker(next)
            .send()
            .await
            .map_err(sdk_err)?;
        let maps = out
            .event_source_mappings()
            .iter()
            .map(|m| SourceMapping {
                source_arn: s(m.event_source_arn()),
                function_arn: s(m.function_arn()),
                state: s(m.state()),
                last_result: s(m.last_processing_result()),
                batch: m.batch_size(),
            })
            .collect();
        Ok((maps, out.next_marker))
    }

    async fn events_buses(&self, next: Option<String>) -> Page<String> {
        let out = self.events.list_event_buses().set_next_token(next).send().await.map_err(sdk_err)?;
        Ok((out.event_buses().iter().map(|b| s(b.name())).collect(), out.next_token))
    }

    async fn events_rules(&self, bus: &str, next: Option<String>) -> Page<Rule> {
        let out = self
            .events
            .list_rules()
            .event_bus_name(bus)
            .set_next_token(next)
            .send()
            .await
            .map_err(sdk_err)?;
        let rules = out
            .rules()
            .iter()
            .map(|r| Rule {
                name: s(r.name()),
                schedule: s(r.schedule_expression()),
                state: r.state().map(|x| x.as_str().to_string()).unwrap_or_default(),
            })
            .collect();
        Ok((rules, out.next_token))
    }

    async fn events_targets(&self, bus: &str, rule: &str, next: Option<String>) -> Page<Target> {
        let out = self
            .events
            .list_targets_by_rule()
            .event_bus_name(bus)
            .rule(rule)
            .set_next_token(next)
            .send()
            .await
            .map_err(sdk_err)?;
        let targets = out
            .targets()
            .iter()
            .map(|t| Target {
                id: t.id().to_string(),
                arn: t.arn().to_string(),
                input: s(t.input()),
                input_path: t.input_path().map(String::from),
                input_template: t.input_transformer().map(|x| x.input_template().to_string()),
                dlq: t.dead_letter_config().map(|d| s(d.arn())),
            })
            .collect();
        Ok((targets, out.next_token))
    }

    async fn scheduler_groups(&self, next: Option<String>) -> Page<String> {
        let out = self
            .scheduler
            .list_schedule_groups()
            .set_next_token(next)
            .send()
            .await
            .map_err(sdk_err)?;
        Ok((out.schedule_groups().iter().map(|g| s(g.name())).collect(), out.next_token))
    }

    async fn scheduler_schedules(&self, group: &str, next: Option<String>) -> Page<String> {
        let out = self
            .scheduler
            .list_schedules()
            .group_name(group)
            .set_next_token(next)
            .send()
            .await
            .map_err(sdk_err)?;
        Ok((out.schedules().iter().map(|x| s(x.name())).collect(), out.next_token))
    }

    async fn scheduler_get(&self, group: &str, name: &str) -> Result<Schedule, AwsErr> {
        let out = self
            .scheduler
            .get_schedule()
            .group_name(group)
            .name(name)
            .send()
            .await
            .map_err(sdk_err)?;
        Ok(Schedule {
            expression: s(out.schedule_expression()),
            timezone: s(out.schedule_expression_timezone()),
            state: out.state().map(|x| x.as_str().to_string()).unwrap_or_default(),
            target: out.target().map(|t| ScheduleTarget {
                arn: t.arn().to_string(),
                input: s(t.input()),
                role_arn: t.role_arn().to_string(),
                dlq: t.dead_letter_config().map(|d| s(d.arn())),
            }),
        })
    }

    async fn sfn_machines(&self, next: Option<String>) -> Page<Machine> {
        let out = self.sfn.list_state_machines().set_next_token(next).send().await.map_err(sdk_err)?;
        let ms = out
            .state_machines()
            .iter()
            .map(|m| Machine {
                name: m.name().to_string(),
                arn: m.state_machine_arn().to_string(),
                kind: m.r#type().as_str().to_string(),
                created: time(Some(m.creation_date())),
            })
            .collect();
        Ok((ms, out.next_token))
    }

    async fn sfn_executions(&self, arn: &str, max: i32) -> Page<Execution> {
        let out = self
            .sfn
            .list_executions()
            .state_machine_arn(arn)
            .max_results(max)
            .send()
            .await
            .map_err(sdk_err)?;
        let es = out
            .executions()
            .iter()
            .map(|e| Execution {
                name: e.name().to_string(),
                status: e.status().as_str().to_string(),
                start: time(Some(e.start_date())),
                stop: time(e.stop_date()),
            })
            .collect();
        Ok((es, out.next_token))
    }

    async fn s3_buckets(&self, next: Option<String>) -> Page<(String, String)> {
        let out = self
            .s3
            .list_buckets()
            .max_buckets(1000)
            .set_continuation_token(next)
            .send()
            .await
            .map_err(sdk_err)?;
        Ok((
            out.buckets().iter().map(|b| (s(b.name()), s(b.bucket_region()))).collect(),
            out.continuation_token,
        ))
    }

    async fn s3_list(&self, bucket: &str, prefix: &str, region: Option<&str>) -> Result<Listing, AwsErr> {
        let out = self
            .s3
            .list_objects_v2()
            .bucket(bucket)
            .prefix(prefix)
            .delimiter("/")
            .max_keys(super::S3_LEVEL)
            .customize()
            .config_override(s3_region(region))
            .send()
            .await
            .map_err(sdk_err)?;
        Ok(Listing {
            prefixes: out.common_prefixes().iter().map(|p| s(p.prefix())).collect(),
            objects: out
                .contents()
                .iter()
                .map(|o| Object {
                    key: s(o.key()),
                    size: o.size().unwrap_or(0),
                    modified: time(o.last_modified()),
                })
                .collect(),
            truncated: out.is_truncated().unwrap_or(false),
        })
    }

    async fn s3_location(&self, bucket: &str) -> Result<String, AwsErr> {
        let out = self.s3.get_bucket_location().bucket(bucket).send().await.map_err(sdk_err)?;
        Ok(out.location_constraint().map(|l| l.as_str().to_string()).unwrap_or_default())
    }

    async fn s3_head(&self, bucket: &str, key: &str, region: Option<&str>) -> Result<Head, AwsErr> {
        let h = self
            .s3
            .head_object()
            .bucket(bucket)
            .key(key)
            .customize()
            .config_override(s3_region(region))
            .send()
            .await
            .map_err(sdk_err)?;
        Ok(Head {
            size: h.content_length().unwrap_or(0),
            modified: time(h.last_modified()),
            content_type: s(h.content_type()),
            content_encoding: s(h.content_encoding()),
            etag: s(h.e_tag()),
            storage_class: h.storage_class().map(|x| x.as_str().to_string()).unwrap_or_default(),
            sse: h.server_side_encryption().map(|x| x.as_str().to_string()).unwrap_or_default(),
            version_id: s(h.version_id()),
            metadata: h.metadata().cloned().unwrap_or_default(),
        })
    }

    async fn s3_get_range(&self, bucket: &str, key: &str, range: &str, region: Option<&str>) -> Result<Vec<u8>, AwsErr> {
        let out = self
            .s3
            .get_object()
            .bucket(bucket)
            .key(key)
            .range(range)
            .customize()
            .config_override(s3_region(region))
            .send()
            .await
            .map_err(sdk_err)?;
        let data = out.body.collect().await.map_err(|e| AwsErr::other(e.to_string()))?;
        Ok(data.into_bytes().to_vec())
    }
}
