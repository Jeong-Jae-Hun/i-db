//! AWS 종류(DynamoDB·Athena·aws 트리)가 같이 쓰는 SDK 설정과 오류 줄이기다.
//! 자격 증명은 저장하지 않고 접속의 profile(없으면 표준 체인)과 region 으로 받는다(R60.1). 실제 연결은 첫 호출 때 한다.

use std::fmt;
use std::time::Duration;

use aws_config::{BehaviorVersion, Region, SdkConfig};
use aws_smithy_http_client::tls;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use aws_smithy_types::timeout::TimeoutConfig;
use idb_core::project::Conn;

/// SDK 설정을 만든다. HTTP 클라이언트는 rustls(ring)이고 리다이렉트를 따라가지 않는다(3xx 는 SDK 가 오류로 낸다).
/// 요청 하나는 Go 저장소 HTTP 클라이언트처럼 2분에서 끊는다.
pub(crate) async fn sdk_config(c: &Conn) -> SdkConfig {
    let http = aws_smithy_http_client::Builder::new()
        .tls_provider(tls::Provider::Rustls(tls::rustls_provider::CryptoMode::Ring))
        .build_https();
    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .http_client(http)
        .timeout_config(TimeoutConfig::builder().operation_timeout(Duration::from_secs(120)).build());
    if !c.region.is_empty() {
        loader = loader.region(Region::new(c.region.clone()));
    }
    if !c.profile.is_empty() {
        loader = loader.profile_name(&c.profile);
    }
    loader.load().await
}

/// AWS 호출 오류를 줄인 것이다. 서비스가 답한 오류는 코드·메시지를, 그 밖(연결·자격 증명)은 원인 글을 싣는다.
/// 요청 서명·자격 증명 값은 싣지 않는다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwsErr {
    /// 서비스 오류 코드(AccessDeniedException 등). 비면 서비스 답이 아니다
    pub code: String,
    pub message: String,
}

impl AwsErr {
    pub fn api(code: &str, message: &str) -> AwsErr {
        AwsErr {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn other(message: impl Into<String>) -> AwsErr {
        AwsErr {
            code: String::new(),
            message: message.into(),
        }
    }

    /// Go 의 `<서비스> <작업>: <코드>: <메시지>` 꼴 조각이다(코드가 없으면 원인 글).
    pub fn text(&self) -> String {
        if self.code.is_empty() {
            self.message.clone()
        } else {
            format!("{}: {}", self.code, self.message)
        }
    }
}

impl fmt::Display for AwsErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text())
    }
}

/// SDK 오류를 AwsErr 로 바꾼다. 서비스 오류면 코드·메시지, 아니면 원인 사슬을 이은 글이다.
pub(crate) fn sdk_err<E, R>(e: aws_smithy_runtime_api::client::result::SdkError<E, R>) -> AwsErr
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
    R: fmt::Debug + Send + Sync + 'static,
{
    if let aws_smithy_runtime_api::client::result::SdkError::ServiceError(se) = &e {
        let err = se.err();
        if let Some(code) = err.code() {
            return AwsErr::api(code, err.message().unwrap_or_default());
        }
    }
    AwsErr::other(chain_text(&e))
}

/// 오류와 원인들을 `: ` 로 잇는다(Go 의 %w 사슬 글과 비슷하게).
fn chain_text(e: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![e.to_string()];
    let mut cur = e.source();
    while let Some(s) = cur {
        let t = s.to_string();
        if !parts.last().is_some_and(|p| p.contains(&t)) {
            parts.push(t);
        }
        cur = s.source();
    }
    parts.join(": ")
}
