//! OpenSearch 접속을 읽기 전용으로 다룬다(명세 R60·R61). Go `internal/opensearch` 를 옮긴다.
//! 트리는 인덱스·별칭 → 매핑 필드이고, 편집기는 SQL 플러그인 SELECT 와 허용한 REST 읽기 경로만 보낸다.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use idb_core::gojson::{json_error, write_str};
use idb_core::project::{Conn, go_quote};
use idb_core::redact::redact;
use idb_core::result::{MAX_ROWS, NULL_TEXT, QueryResult, cap_cell};
use idb_core::sqlsafe::first_sql_word;
use idb_core::textutil::{go_upper, korean_count};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;
use tokio::time::Instant;

use crate::Error;
use crate::httpc::{self, SendError, Values, encode, escape_path, parse_query, path_escape};
use crate::source::{Health, MAX_BODY, Metric, Node, OPEN_LIMIT, Query, sql_words};

/// OpenSearch 기본 index.max_result_window 다. 넘는 size·from+size 는 확인을 받는다(R60.2)
const MAX_WINDOW: usize = 10000;
/// 트리 Enter 미리 보기(open)의 본문이다
const OPEN_BODY: &str = r#"{"query":{"match_all":{}},"size":%d}"#;
const UNREADABLE_WINDOW: &str = "size·from 을 읽을 수 없음";

pub struct OpenSearch {
    /// scheme://host[:port]
    origin: String,
    /// 주소의 경로(풀어 쓴 꼴, 끝 '/' 없음)
    base_path: String,
    hc: reqwest::Client,
    user: String,
    password: String,
    /// 주소에 든 user:pass 다. 다른 인증이 없을 때 Go http.Client 처럼 basic 으로 보낸다
    url_user: Option<(String, String)>,
    sigv4: Option<SigV4>,
    /// 정렬·거르기 다시 읽기(requery)가 읽은 인덱스 매핑이다. 접속이 살아 있는 동안 둔다
    fields: Mutex<HashMap<String, Arc<Vec<Field>>>>,
}

struct SigV4 {
    creds: SharedCredentialsProvider,
    region: String,
}

impl OpenSearch {
    /// 접속 하나를 만든다. c.password 는 호출부가 .env 에서 고른 basic 비밀번호다.
    /// 네트워크는 첫 요청 때 처음 쓴다(sigv4 자격 증명도 그때 받는다). sigv4 설정(리전·프로필)은 여기서 읽는다.
    pub async fn new(c: &Conn) -> Result<OpenSearch, Error> {
        let trimmed = c.url.trim_end_matches('/');
        let invalid = || Error::msg(format!("opensearch: invalid url {} (need http(s)://host)", go_quote(&c.url)));
        let u = url::Url::parse(trimmed).map_err(|_| invalid())?;
        if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none_or(str::is_empty) {
            return Err(invalid());
        }
        let decode = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned();
        let url_user =
            (!u.username().is_empty() || u.password().is_some()).then(|| (decode(u.username()), decode(u.password().unwrap_or(""))));
        let mut b = OpenSearch {
            origin: format!("{}://{}", u.scheme(), &u[url::Position::BeforeHost..url::Position::AfterPort]),
            base_path: decode(u.path()).trim_end_matches('/').to_string(),
            hc: httpc::client().map_err(|e| Error::msg(format!("opensearch: {e}")))?,
            user: String::new(),
            password: String::new(),
            url_user,
            sigv4: None,
            fields: Mutex::new(HashMap::new()),
        };
        match c.auth.as_str() {
            "" => {}
            "basic" => {
                if c.user.is_empty() {
                    return Err(Error::msg("opensearch: basic auth needs user"));
                }
                b.user = c.user.clone();
                b.password = c.password.clone();
            }
            "sigv4" => b.sigv4 = Some(load_sigv4(c).await?),
            other => {
                return Err(Error::msg(format!(
                    "opensearch: unsupported auth {} (basic, sigv4 or none)",
                    go_quote(other)
                )));
            }
        }
        Ok(b)
    }

    // ─── 요청 ───

    /// path(질의 문자열 포함 가능)에 요청을 보내고 2xx 본문을 돌려준다.
    /// 오류는 OpenSearch 의 error.type·reason 을 싣고 비밀번호는 지운다.
    async fn send(&self, method: &str, path: &str, body: &[u8]) -> Result<Vec<u8>, Error> {
        let path = path.trim_start_matches('/');
        let (p, raw_query) = path.split_once('?').unwrap_or((path, ""));
        let mut target = self.origin.clone() + &escape_path(&format!("{}/{p}", self.base_path));
        if !raw_query.is_empty() {
            target = format!("{target}?{raw_query}");
        }
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        if !body.is_empty() {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        if !self.user.is_empty() {
            headers.insert(AUTHORIZATION, basic_auth(&self.user, &self.password));
        } else if let Some(s) = &self.sigv4 {
            s.sign(method, &target, &mut headers, body).await?;
        } else if let Some((u, p)) = &self.url_user {
            headers.insert(AUTHORIZATION, basic_auth(u, p));
        }
        let resp = match httpc::send(&self.hc, method, &target, headers, body.to_vec()).await {
            Ok(r) => r,
            Err(SendError::Transport(e)) => return Err(Error::msg(format!("opensearch: {}", redact(&e, &self.password)))),
            Err(SendError::Read(e)) => return Err(Error::msg(format!("opensearch: read response: {}", redact(&e, &self.password)))),
        };
        if let Some(e) = resp.redirect_error() {
            return Err(Error::msg(format!("opensearch: {e}")));
        }
        if resp.body.len() > MAX_BODY {
            return Err(Error::msg(format!(
                "opensearch: response over {} MiB, narrow the query",
                MAX_BODY >> 20
            )));
        }
        if resp.status / 100 != 2 {
            return Err(Error::msg(format!(
                "opensearch {}: {}",
                resp.status,
                redact(&error_text(&resp.body), &self.password)
            )));
        }
        Ok(resp.body)
    }

    async fn get(&self, path: &str) -> Result<Vec<u8>, Error> {
        self.send("GET", path, &[]).await
    }

    // ─── 트리 ───

    pub async fn children(&self, path: &str) -> Result<Vec<Node>, Error> {
        if path.is_empty() {
            return self.indices().await;
        }
        if path.contains('/') {
            return Ok(Vec::new()); // 필드는 잎이다
        }
        let fields = self.mapping(path).await?;
        Ok(fields
            .iter()
            .map(|f| Node {
                name: f.name.clone(),
                path: format!("{path}/{}", f.name),
                info: f.typ.clone(),
                ..Default::default()
            })
            .collect())
    }

    /// 인덱스와 별칭(`별칭 → 인덱스`)을 이름순으로 돌려준다. `.` 으로 시작하는 시스템 인덱스·별칭은 숨긴다.
    async fn indices(&self) -> Result<Vec<Node>, Error> {
        #[derive(Deserialize)]
        struct Index {
            #[serde(default, deserialize_with = "nullable")]
            index: String,
            #[serde(default, rename = "docs.count", deserialize_with = "nullable")]
            docs: String,
            #[serde(default, deserialize_with = "nullable")]
            health: String,
        }
        #[derive(Deserialize)]
        struct Alias {
            #[serde(default, deserialize_with = "nullable")]
            alias: String,
            #[serde(default, deserialize_with = "nullable")]
            index: String,
        }
        let data = self.get("_cat/indices?format=json&h=index,docs.count,health").await?;
        let idx: Option<Vec<Index>> = decode(&data, "_cat/indices")?;
        let data = self.get("_cat/aliases?format=json&h=alias,index").await?;
        let aliases: Option<Vec<Alias>> = decode(&data, "_cat/aliases")?;
        let mut nodes: Vec<Node> = idx
            .unwrap_or_default()
            .into_iter()
            .filter(|i| !i.index.starts_with('.'))
            .map(|i| Node {
                name: i.index.clone(),
                path: i.index,
                info: format!("{} {}", i.docs, i.health).trim().to_string(),
                has_children: true,
                openable: true,
                ..Default::default()
            })
            .collect();
        // 별칭 하나가 인덱스 여럿을 가리키면 한 줄에 모은다
        let mut targets: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for a in aliases.unwrap_or_default() {
            if !a.alias.starts_with('.') {
                targets.entry(a.alias).or_default().push(a.index);
            }
        }
        for (alias, mut ix) in targets {
            ix.sort();
            nodes.push(Node {
                name: format!("{alias} → {}", ix.join(", ")),
                path: alias,
                info: "alias".into(),
                has_children: true,
                openable: true,
                ..Default::default()
            });
        }
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(nodes)
    }

    /// 인덱스(별칭·패턴이면 대상 전부)의 매핑 필드를 `a.b` 로 펴서 이름순으로 돌려준다.
    /// 같은 필드가 인덱스마다 타입이 다르면 이름순 첫 인덱스의 타입을 쓴다.
    async fn mapping(&self, index: &str) -> Result<Vec<Field>, Error> {
        let data = self.get(&format!("{}/_mapping", path_escape(index))).await?;
        let m: Option<BTreeMap<String, IndexMapping>> = decode(&data, "_mapping")?;
        let (mut seen, mut kws) = (HashMap::new(), HashMap::new());
        for im in m.unwrap_or_default().values() {
            flatten(&mut seen, "", &im.mappings.properties);
            keywords(&mut kws, "", &im.mappings.properties);
        }
        let mut fields: Vec<Field> = seen
            .into_iter()
            .map(|(name, typ)| Field {
                keyword: kws.get(&name).cloned().unwrap_or_default(),
                name,
                typ,
            })
            .collect();
        fields.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(fields)
    }

    // ─── 실행 ───

    pub async fn open(&self, path: &str, limit: usize) -> Result<QueryResult, Error> {
        let limit = if limit == 0 { OPEN_LIMIT } else { limit };
        let index = path.split('/').next().unwrap_or("");
        let start = Instant::now();
        let body = OPEN_BODY.replace("%d", &limit.to_string());
        let data = self
            .send("POST", &format!("{}/_search", path_escape(index)), body.as_bytes())
            .await?;
        let mut res = search_result(&data, limit)?;
        res.elapsed = start.elapsed();
        Ok(res)
    }

    /// SELECT 면 SQL 플러그인으로, `GET|POST <경로>` 첫 줄이면 허용한 REST 읽기 경로로 보낸다(R61.2).
    /// 그 밖은 보내지 않고 Error::Write 다.
    pub async fn run(&self, q: &Query) -> Result<QueryResult, Error> {
        let limit = if q.limit == 0 { MAX_ROWS } else { q.limit };
        let start = Instant::now();
        let mut res = if first_sql_word(&q.text) == "select" {
            self.run_sql(&q.text, limit).await?
        } else {
            self.run_rest(q, limit).await?
        };
        res.elapsed = start.elapsed();
        Ok(res)
    }

    async fn run_sql(&self, text: &str, limit: usize) -> Result<QueryResult, Error> {
        let stmt = sql_statement(text)?;
        // fetch_size 를 주면 플러그인 기본 상한(plugins.query.size_limit, 200행) 대신 첫 페이지로 limit 행까지 돌려준다.
        // 다음 페이지 커서는 따라가지 않는다(행 상한이 곧 첫 페이지 크기다)
        let body = format!(r#"{{"fetch_size":{},"query":{}}}"#, limit.min(MAX_WINDOW), go_json_str(&stmt));
        let data = self.send("POST", "_plugins/_sql?format=jdbc", body.as_bytes()).await?;
        #[derive(Deserialize)]
        struct Schema {
            #[serde(default, deserialize_with = "nullable")]
            name: String,
            #[serde(default, deserialize_with = "nullable")]
            alias: String,
        }
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            schema: Option<Vec<Schema>>,
            #[serde(default)]
            datarows: Option<Vec<Option<Vec<Box<RawValue>>>>>,
        }
        let r: Resp =
            serde_json::from_slice(&data).map_err(|e| Error::msg(format!("opensearch: decode sql response: {}", json_error(&e))))?;
        let mut res = QueryResult {
            cols: r
                .schema
                .unwrap_or_default()
                .into_iter()
                .map(|s| if s.alias.is_empty() { s.name } else { s.alias })
                .collect(),
            ..Default::default()
        };
        let mut rows = r.datarows.unwrap_or_default();
        if rows.len() > limit {
            rows.truncate(limit);
            res.truncated = true;
        }
        for dr in rows {
            let dr = dr.unwrap_or_default();
            let row = (0..res.cols.len())
                .map(|i| dr.get(i).map_or_else(|| NULL_TEXT.to_string(), |v| cell(v.get())))
                .collect();
            res.rows.push(row);
        }
        Ok(res)
    }

    async fn run_rest(&self, q: &Query, limit: usize) -> Result<QueryResult, Error> {
        let Some((method, path, body)) = parse_request(&q.text) else {
            return Err(Error::Write("expected SELECT … or GET <index>/_search".into()));
        };
        let path = path.trim_start_matches('/');
        let (p, raw_query) = path.split_once('?').unwrap_or((path, ""));
        let (mut params, err) = parse_query(raw_query);
        if let Some(e) = err {
            return Err(Error::msg(format!("opensearch: invalid query string: {e}")));
        }
        let Some(endpoint) = allowed_path(&method, p) else {
            return Err(Error::Write(format!("{method} /{p}")));
        };
        if !body.is_empty() && serde_json::from_str::<de::IgnoredAny>(&body).is_err() {
            return Err(Error::msg("opensearch: request body is not valid JSON"));
        }
        if endpoint == "_search" && !q.force {
            check_window(&body, &params)?;
        }
        if endpoint == "_cat" {
            params.insert("format".into(), vec!["json".into()]); // 표로 만들려면 JSON 이어야 한다
        }
        let mut target = p.to_string();
        if !params.is_empty() {
            target = format!("{target}?{}", encode(&params));
        }
        let data = self.send(&method, &target, body.as_bytes()).await?;
        match endpoint {
            "_search" => search_result(&data, limit),
            "_count" => {
                #[derive(Deserialize)]
                struct Count {
                    #[serde(default, deserialize_with = "raw_some")]
                    count: Option<Box<RawValue>>,
                }
                let r: Count = decode::<Option<Count>>(&data, "_count")?.unwrap_or(Count { count: None });
                Ok(QueryResult {
                    cols: vec!["count".into()],
                    rows: vec![vec![cell(raw_text(&r.count))]],
                    ..Default::default()
                })
            }
            "_mapping" => {
                let m: Option<BTreeMap<String, IndexMapping>> = decode(&data, "_mapping")?;
                let mut res = QueryResult {
                    cols: ["index", "field", "type"].map(String::from).to_vec(),
                    ..Default::default()
                };
                for (index, im) in m.unwrap_or_default() {
                    let mut seen = HashMap::new();
                    flatten(&mut seen, "", &im.mappings.properties);
                    let mut names: Vec<_> = seen.into_iter().collect();
                    names.sort();
                    for (name, typ) in names {
                        res.rows.push(vec![index.clone(), name, typ]);
                    }
                }
                if res.rows.len() > limit {
                    res.rows.truncate(limit);
                    res.truncated = true;
                }
                Ok(res)
            }
            _ => {
                let items: Option<Vec<Box<RawValue>>> = decode(&data, "_cat")?;
                let items = items.unwrap_or_default();
                object_table(&[], &items.iter().map(|r| r.get()).collect::<Vec<_>>(), limit)
            }
        }
    }

    // ─── 대시보드 확인 (R64.2) ───

    /// `_cluster/health` 상태·`_cat/indices` 인덱스 수와 문서 합·`GET /` 버전이다.
    /// 편집기 허용 경로(R61.2)를 거치지 않는 내부 읽기다. 지연은 `_cluster/health` 왕복이고, `green` 이 아니면 경고다.
    /// 인덱스·문서 수는 트리처럼 `.` 으로 시작하는 시스템 인덱스를 뺀다. 버전은 못 읽어도(권한) 실패로 보지 않는다.
    pub async fn probe(&self) -> Health {
        let start = Instant::now();
        let data = self.get("_cluster/health").await;
        let latency = start.elapsed();
        let fail = |e: &str| Health::failed(latency, e, &self.password);
        let data = match data {
            Ok(d) => d,
            Err(e) => return fail(&e.to_string()),
        };
        #[derive(Deserialize)]
        struct ClusterHealth {
            #[serde(default, deserialize_with = "nullable")]
            status: String,
        }
        let status = match serde_json::from_slice::<ClusterHealth>(&data) {
            Ok(h) if !h.status.is_empty() => h.status,
            _ => return fail("opensearch: unexpected _cluster/health response"),
        };
        #[derive(Deserialize)]
        struct Index {
            #[serde(default, deserialize_with = "nullable")]
            index: String,
            #[serde(default, rename = "docs.count", deserialize_with = "nullable")]
            docs: String,
        }
        let idx: Vec<Index> = match self.get("_cat/indices?format=json&h=index,docs.count").await {
            Err(e) => return fail(&e.to_string()),
            Ok(d) => match decode::<Option<Vec<Index>>>(&d, "_cat/indices") {
                Ok(v) => v.unwrap_or_default(),
                Err(e) => return fail(&e.to_string()),
            },
        };
        let (mut indices, mut docs) = (0i64, 0i64);
        for i in idx.iter().filter(|i| !i.index.starts_with('.')) {
            indices += 1;
            docs += i.docs.parse::<i64>().unwrap_or(0); // 닫힌 인덱스는 docs.count 가 비어 있다
        }
        Health {
            ok: true,
            latency,
            version: self.version().await,
            metrics: vec![
                Metric::new("", status.clone(), status != "green"),
                Metric::new("문서", korean_count(docs), false),
                Metric::new("인덱스", korean_count(indices), false),
            ],
            err: String::new(),
        }
    }

    /// `GET /` 의 배포판·버전이다(`OpenSearch 2.17.0`, distribution 이 없으면 Elasticsearch). 못 읽으면 빈 글이다.
    async fn version(&self) -> String {
        #[derive(Deserialize, Default)]
        struct Version {
            #[serde(default, deserialize_with = "nullable")]
            distribution: String,
            #[serde(default, deserialize_with = "nullable")]
            number: String,
        }
        #[derive(Deserialize)]
        struct Root {
            #[serde(default)]
            version: Option<Version>,
        }
        let Ok(data) = self.get("").await else { return String::new() };
        let v = match serde_json::from_slice::<Root>(&data) {
            Ok(r) => r.version.unwrap_or_default(),
            Err(_) => return String::new(),
        };
        match (v.number.is_empty(), v.distribution == "opensearch") {
            (true, _) => String::new(),
            (false, true) => format!("OpenSearch {}", v.number),
            (false, false) => format!("Elasticsearch {}", v.number),
        }
    }

    // ─── 정렬·거르기 서버로 다시 읽기 (R61.3) ───

    /// _search 요청 q.text 의 sort·query 를 r 로 바꿔 보내고, 보낸 글을 같이 돌려준다.
    /// 인덱스 매핑을 읽어(인덱스마다 접속이 살아 있는 동안 한 번) keyword 하위 필드가 있는
    /// text 필드는 그 하위 필드로 정렬·비교한다. 매핑을 못 읽으면 필드 이름 그대로 보낸다.
    pub async fn requery(&self, q: &Query, r: &Rewrite) -> Result<(String, QueryResult), Error> {
        let fields = self.field_info(&q.text).await;
        let info = |name: &str| match fields
            .as_ref()
            .and_then(|f| f.binary_search_by(|x| x.name.as_str().cmp(name)).ok().map(|i| &f[i]))
        {
            Some(f) => (f.typ.clone(), f.keyword.clone()),
            None => (String::new(), String::new()),
        };
        let Some(text) = rewrite(&q.text, r, &info) else {
            return Err(Error::msg("opensearch: not a _search request with a JSON object body"));
        };
        let q = Query {
            text: text.clone(),
            ..q.clone()
        };
        Ok((text, self.run(&q).await?))
    }

    /// 요청 경로의 인덱스 매핑이다. 인덱스 없는 `_search` 는 모든 인덱스라 읽지 않는다.
    async fn field_info(&self, text: &str) -> Option<Arc<Vec<Field>>> {
        let (_, path, _) = parse_request(text)?;
        let path = path.trim_start_matches('/');
        let p = path.split('?').next().unwrap_or("");
        let (index, _) = p.split_once('/').filter(|(i, _)| !i.is_empty())?;
        if let Some(f) = self.fields.lock().ok()?.get(index) {
            return Some(f.clone());
        }
        let fields = Arc::new(self.mapping(index).await.ok()?);
        self.fields.lock().ok()?.insert(index.to_string(), fields.clone());
        Some(fields)
    }
}

/// Go `config.LoadDefaultConfig` 처럼 기본 체인(환경·공유 설정·SSO·프로세스·IMDS)에서 리전·자격 증명을 고른다.
async fn load_sigv4(c: &Conn) -> Result<SigV4, Error> {
    let http = aws_smithy_http_client::Builder::new()
        .tls_provider(aws_smithy_http_client::tls::Provider::Rustls(
            aws_smithy_http_client::tls::rustls_provider::CryptoMode::Ring,
        ))
        .build_https();
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest()).http_client(http);
    if !c.region.is_empty() {
        loader = loader.region(aws_config::Region::new(c.region.clone()));
    }
    if !c.profile.is_empty() {
        loader = loader.profile_name(&c.profile);
    }
    let cfg = loader.load().await;
    let region = cfg.region().map(|r| r.to_string()).unwrap_or_default();
    if region.is_empty() {
        return Err(Error::msg("opensearch: sigv4 needs region (connection region or profile)"));
    }
    let creds = cfg
        .credentials_provider()
        .ok_or_else(|| Error::msg("opensearch: load aws config: no credentials provider"))?;
    Ok(SigV4 { creds, region })
}

impl SigV4 {
    /// headers 에 SigV4 서명 헤더(Authorization·X-Amz-Date·토큰)를 더한다. 서비스 이름은 es 다.
    async fn sign(&self, method: &str, target: &str, headers: &mut HeaderMap, body: &[u8]) -> Result<(), Error> {
        let creds = self
            .creds
            .provide_credentials()
            .await
            .map_err(|e| Error::msg(format!("opensearch: aws credentials: {}", err_chain(&e))))?;
        let identity = creds.into();
        let signing_err = |e: &dyn std::error::Error| Error::msg(format!("opensearch: sigv4 sign: {}", err_chain(e)));
        let params = aws_sigv4::sign::v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.region)
            .name("es")
            .time(SystemTime::now())
            .settings(aws_sigv4::http_request::SigningSettings::default())
            .build()
            .map_err(|e| signing_err(&e))?
            .into();
        let pairs: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let signable = aws_sigv4::http_request::SignableRequest::new(
            method,
            target,
            pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            aws_sigv4::http_request::SignableBody::Bytes(body),
        )
        .map_err(|e| signing_err(&e))?;
        let (instructions, _) = aws_sigv4::http_request::sign(signable, &params)
            .map_err(|e| signing_err(&e))?
            .into_parts();
        for (name, value) in instructions.headers() {
            let (Ok(n), Ok(v)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) else {
                return Err(Error::msg("opensearch: sigv4 sign: invalid header"));
            };
            headers.insert(n, v);
        }
        Ok(())
    }
}

fn err_chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        msg = format!("{msg}: {s}");
        src = s.source();
    }
    msg
}

fn basic_auth(user: &str, pass: &str) -> HeaderValue {
    let v = format!("Basic {}", base64(format!("{user}:{pass}").as_bytes()));
    let mut h = HeaderValue::from_str(&v).unwrap_or_else(|_| HeaderValue::from_static(""));
    h.set_sensitive(true);
    h
}

/// 표준 base64(채움 있음)다. basic 인증 한 곳에만 써서 의존성을 들이지 않는다.
pub(crate) fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for chunk in b.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `{"error":{"type","reason"}}` 또는 `{"error":"..."}` 를 한 줄로 만든다. 모르는 꼴은 본문 앞부분이다.
fn error_text(data: &[u8]) -> String {
    if let Ok(serde_json::Value::Object(top)) = serde_json::from_slice::<serde_json::Value>(data)
        && let Some(e) = top.get("error")
    {
        let field = |o: &serde_json::Map<String, serde_json::Value>, k: &str| match o.get(k) {
            None | Some(serde_json::Value::Null) => Some(String::new()),
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(_) => None,
        };
        match e {
            serde_json::Value::Object(o) => {
                if let (Some(typ), Some(reason), Some(details)) = (field(o, "type"), field(o, "reason"), field(o, "details"))
                    && (!typ.is_empty() || !reason.is_empty())
                {
                    let mut msg = format!("{typ}: {reason}");
                    if let Some(m) = msg.strip_prefix(": ") {
                        msg = m.to_string();
                    }
                    // SQL 플러그인 details 는 둘째 줄부터 안내 문구라 첫 줄만 싣는다
                    let d = details.split('\n').next().unwrap_or("");
                    if !d.is_empty() && d != reason {
                        msg = format!("{msg} ({d})");
                    }
                    return msg;
                }
            }
            serde_json::Value::String(s) => return s.clone(),
            serde_json::Value::Null => return String::new(),
            _ => {}
        }
    }
    let s = String::from_utf8_lossy(data);
    let s = s.trim();
    match httpc::head(s, 300) {
        Some(h) => h + "…",
        None => s.to_string(),
    }
}

// ─── 매핑 ───

/// 매핑 필드 하나다. keyword 는 text 필드의 keyword 하위 필드 이름(`name.keyword` 꼴)이고 없으면 비었다.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Field {
    name: String,
    typ: String,
    keyword: String,
}

#[derive(Deserialize, Default)]
struct IndexMapping {
    #[serde(default, deserialize_with = "null_default")]
    mappings: MappingProps,
}

#[derive(Deserialize, Default)]
struct MappingProps {
    #[serde(default, rename = "type", deserialize_with = "nullable")]
    typ: String,
    #[serde(default, deserialize_with = "null_default")]
    properties: BTreeMap<String, MappingProps>,
    /// 다중 필드(text 의 keyword 등)
    #[serde(default, deserialize_with = "null_default")]
    fields: BTreeMap<String, MappingProps>,
}

/// properties 를 `a.b` 이름 → 타입으로 편다. 타입이 없는 object 는 줄을 만들지 않고 아래만 편다.
fn flatten(out: &mut HashMap<String, String>, prefix: &str, props: &BTreeMap<String, MappingProps>) {
    for (name, p) in props {
        let full = format!("{prefix}{name}");
        if !out.contains_key(&full) && !p.typ.is_empty() {
            out.insert(full.clone(), p.typ.clone());
        }
        flatten(out, &format!("{full}."), &p.properties);
    }
}

/// text 필드마다 keyword 타입 하위 필드 `이름.<하위>` 를 적는다. 여럿이면 이름이 "keyword" 인 것, 없으면 이름순 첫 것이다.
fn keywords(out: &mut HashMap<String, String>, prefix: &str, props: &BTreeMap<String, MappingProps>) {
    for (name, p) in props {
        let full = format!("{prefix}{name}");
        if !out.contains_key(&full) && p.typ == "text" {
            for (sub, sp) in &p.fields {
                if sp.typ == "keyword" && (out.get(&full).is_none_or(String::is_empty) || sub == "keyword") {
                    out.insert(full.clone(), format!("{full}.{sub}"));
                }
            }
        }
        keywords(out, &format!("{full}."), &p.properties);
    }
}

// ─── 요청 글 ───

/// 트리 Enter 미리 보기(open)와 같은 요청을 편집기 꼴(`POST <인덱스>/_search` + 본문)로 쓴 글이다.
/// 미리 보기 결과도 정렬을 서버로 다시 읽을 수 있게 결과의 원래 요청으로 남긴다(R61.3).
pub fn open_text(path: &str, limit: usize) -> String {
    let limit = if limit == 0 { OPEN_LIMIT } else { limit };
    let index = path.split('/').next().unwrap_or("");
    format!("POST {index}/_search\n{}", OPEN_BODY.replace("%d", &limit.to_string()))
}

/// 빈 줄·주석 줄(`--`·`//`·`#`)을 건너뛴 첫 줄을 `메서드 경로` 로, 나머지를 JSON 본문으로 나눈다.
fn parse_request(text: &str) -> Option<(String, String, String)> {
    let lines: Vec<&str> = text.split('\n').collect();
    for (i, line) in lines.iter().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("--") || line.starts_with("//") || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() != 2 {
            return None;
        }
        return Some((go_upper(f[0]), f[1].to_string(), lines[i + 1..].join("\n").trim().to_string()));
    }
    None
}

/// `<인덱스>/<끝>` 꼴로 GET 할 수 있는 읽기 끝점이다. POST 는 _search 만 받는다
const SEARCH_ENDPOINTS: [&str; 3] = ["_search", "_count", "_mapping"];

/// 읽기로 허용한 경로면 결과 꼴을 가를 끝점 이름이다.
/// GET: `_search`, `_cat/<이름>[/<인덱스>]`, `<인덱스>/_search|_count|_mapping`. POST: `_search`, `<인덱스>/_search`.
/// 마디는 비거나 `.`·`..`·`%`·제어 문자를 가질 수 없다(서버·프록시가 경로를 다시 풀어 다른 API 로 가는 것을 막는다).
fn allowed_path(method: &str, p: &str) -> Option<&'static str> {
    let segs: Vec<&str> = p.split('/').collect();
    if method != "GET" && method != "POST" || segs.iter().any(|s| bad_segment(s)) {
        return None;
    }
    if segs.len() == 1 && segs[0] == "_search" {
        return Some("_search");
    }
    if segs[0] == "_cat" {
        let ok = method == "GET" && (2..=3).contains(&segs.len()) && cat_name(segs[1]) && (segs.len() == 2 || index_ok(segs[2]));
        return ok.then_some("_cat");
    }
    if segs.len() != 2 || !index_ok(segs[0]) {
        return None;
    }
    if method == "POST" {
        return (segs[1] == "_search").then_some("_search");
    }
    SEARCH_ENDPOINTS.iter().find(|e| **e == segs[1]).copied()
}

fn cat_name(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b == b'_' || b.is_ascii_alphanumeric())
}

fn bad_segment(s: &str) -> bool {
    s.is_empty() || s == "." || s == ".." || s.contains(['%', '\\']) || s.chars().any(|c| (c as u32) < 0x20 || c == '\x7f')
}

/// 인덱스 자리(쉼표 목록)인지 본다. `_` 로 시작하면 안 되고(`_all` 만 받음) `_doc`·`_bulk` 같은 API 를
/// 인덱스로 착각하지 않게 한다. `.` 으로 시작하는 시스템 인덱스(`.opendistro_security` 등)는 받지 않는다(R61.2).
fn index_ok(s: &str) -> bool {
    s.split(',').all(|part| {
        let part = part.strip_prefix('-').unwrap_or(part); // 빼기 꼴(-logs-old)도 같은 규칙
        !(part.is_empty() || part.starts_with('.') || part.starts_with('_') && part != "_all")
    })
}

/// text 가 허용한 `_search` 요청 글인지다. SELECT·_count·_cat·_mapping 은 아니다.
pub fn is_search(text: &str) -> bool {
    let Some((method, path, _)) = parse_request(text) else {
        return false;
    };
    let path = path.trim_start_matches('/');
    let p = path.split('?').next().unwrap_or("");
    allowed_path(&method, p) == Some("_search")
}

/// SQL 플러그인에 보내지 않을 쓰기 낱말이다. 문자열·주석 밖이면 어디 있든(하위 질의 포함) 거절한다.
/// 백틱 이름(`delete`)도 걸리지만 안전 쪽이다.
const SQL_WRITE_WORDS: [&str; 8] = ["delete", "update", "insert", "drop", "create", "alter", "truncate", "replace"];

/// SELECT 한 문장을 보낼 글로 다듬는다. 여러 문장(sql_words)이거나 쓰기 낱말이 있으면 Error::Write 다(R61.2).
fn sql_statement(text: &str) -> Result<String, Error> {
    let (stmt, words) = sql_words(text)?;
    if let Some(w) = words.iter().find(|w| SQL_WRITE_WORDS.contains(&w.as_str())) {
        return Err(Error::Write(w.clone()));
    }
    Ok(stmt)
}

/// 본문·질의 문자열의 size·from 이 MAX_WINDOW 를 넘으면 Error::NeedsConfirm 이다.
/// 숫자는 문자열("20")·실수(2e4)도 OpenSearch 가 받으므로 같이 읽는다. 읽을 수 없거나 같은 키가 둘이면
/// 서버가 어느 값을 쓸지 몰라 확인을 받는다(안전 쪽).
fn check_window(body: &str, params: &Values) -> Result<(), Error> {
    let unreadable = || Error::NeedsConfirm(UNREADABLE_WINDOW.into());
    let mut vals: HashMap<&str, Vec<String>> = HashMap::new();
    if !body.is_empty() {
        let obj = ordered_object(body).map_err(|_| unreadable())?;
        for (k, raw) in &obj {
            if k == "size" || k == "from" {
                vals.entry(if k == "size" { "size" } else { "from" })
                    .or_default()
                    .push(json_scalar(raw.get()));
            }
        }
    }
    let mut window = HashMap::new();
    for k in ["size", "from"] {
        let mut all = vals.remove(k).unwrap_or_default();
        all.extend(params.get(k).cloned().unwrap_or_default());
        if all.len() > 1 {
            return Err(unreadable());
        }
        let Some(v) = all.first() else { continue };
        let v = v.trim();
        // Go ParseFloat 는 16진 실수도 받지만 드문 꼴이라 읽지 않고 확인을 받는다(안전 쪽)
        match v.parse::<f64>() {
            Ok(f) if f.is_finite() && !v.contains(['x', 'X']) => {
                window.insert(k, f);
            }
            _ => return Err(unreadable()),
        }
    }
    let (size, from) = (
        window.get("size").copied().unwrap_or(0.0),
        window.get("from").copied().unwrap_or(0.0),
    );
    if size > MAX_WINDOW as f64 || from + size > MAX_WINDOW as f64 {
        return Err(Error::NeedsConfirm("size 1만 초과".into()));
    }
    Ok(())
}

/// JSON 문자열이면 풀어 쓴 값, 아니면 원문이다(숫자·불·객체는 숫자 읽기가 가른다).
fn json_scalar(raw: &str) -> String {
    serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_string())
}

// ─── 정렬·거르기 글 바꾸기 (R61.3) ───

/// _search 다시 읽기의 거르기 하나다(R14.4 꼴). field 가 비면 모든 필드 찾기다.
/// op 는 "=" "!=" ">" ">=" "<" "<=" ":" "null" "notnull" 이고 모든 필드 찾기는 ":" 또는 "!:" 다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    pub field: String,
    pub op: String,
    pub value: String,
}

/// _search 요청에 바꿔 넣을 정렬·거르기다. sort 가 비면 본문 sort 를, filter 가 None 이면 본문 query 를 그대로 둔다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rewrite {
    pub sort: String,
    pub desc: bool,
    pub filter: Option<Filter>,
}

/// 필드의 매핑 타입과 keyword 하위 필드 이름이다. 모르면 둘 다 빈 글이다.
type FieldInfo<'a> = &'a dyn Fn(&str) -> (String, String);

/// requery 의 글 바꾸기다. 값이 없는 문서는 오름·내림 모두 뒤로 보내고, 거르기는 원래 query 를
/// bool.must 에 두고 조건을 bool.filter(아니다 꼴은 must_not)에 더한다. _search 가 아니거나 본문이 JSON 객체가 아니면 None 이다.
///   - 예: `GET logs/_search` + `{"size":20}`, sort "status" desc → `{"size":20,"sort":[{"status":{"missing":"_last","order":"desc"}}]}`
///   - 예: filter status=500 → `{"query":{"bool":{"filter":[{"term":{"status":"500"}}],"must":[{"match_all":{}}]}}}`
fn rewrite(text: &str, r: &Rewrite, info: FieldInfo) -> Option<String> {
    let (method, path, body) = parse_request(text)?;
    if !is_search(text) {
        return None;
    }
    // Go json.Marshal 처럼 키 이름순으로 쓰고 값(원래 JSON)은 압축한다
    let mut obj: BTreeMap<String, String> = BTreeMap::new();
    if !body.is_empty() {
        let m: Option<BTreeMap<String, Box<RawValue>>> = serde_json::from_str(&body).ok()?;
        for (k, v) in m.unwrap_or_default() {
            obj.insert(k, compact(v.get(), true));
        }
    }
    if !r.sort.is_empty() {
        let order = if r.desc { "desc" } else { "asc" };
        // _score 정렬은 missing 을 받지 않는다(모든 문서에 점수가 있다)
        let key = if r.sort == "_score" {
            format!(r#"{{"order":"{order}"}}"#)
        } else {
            format!(r#"{{"missing":"_last","order":"{order}"}}"#)
        };
        obj.insert("sort".into(), format!("[{{{}:{key}}}]", go_json_str(&exact_field(&r.sort, info))));
    }
    if let Some(f) = &r.filter
        && let Some((clause, not)) = filter_clause(f, info)
    {
        let orig = obj.get("query").cloned().unwrap_or_else(|| r#"{"match_all":{}}"#.into());
        let q = if not {
            format!(r#"{{"bool":{{"must":[{orig}],"must_not":[{clause}]}}}}"#)
        } else {
            format!(r#"{{"bool":{{"filter":[{clause}],"must":[{orig}]}}}}"#)
        };
        obj.insert("query".into(), q);
    }
    let fields: Vec<String> = obj.iter().map(|(k, v)| format!("{}:{v}", go_json_str(k))).collect();
    Some(format!("{method} {path}\n{{{}}}", fields.join(",")))
}

/// text 필드에 keyword 하위 필드가 있으면 그 이름이다. text 는 낱말로 쪼개 저장해 정렬·같음 비교가 안 된다.
fn exact_field(name: &str, info: FieldInfo) -> String {
    match info(name) {
        (typ, kw) if typ == "text" && !kw.is_empty() => kw,
        _ => name.to_string(),
    }
}

/// 거르기를 query 절 JSON 하나로 바꾼다. 둘째 값이 참이면 must_not 자리다. 모든 필드 찾기의 빈 값은 None(거르기 없음)이다.
///   - 예: status=A → term, n>5 → range(십진수면 숫자), msg:err → wildcard *err*(대소문자 무시), msg=null → must_not exists.
///   - 예: 모든 필드 abc → query_string "abc"(따옴표로 감싸 연산자 글자를 막고 lenient 로 타입이 안 맞는 필드는 건너뜀).
fn filter_clause(f: &Filter, info: FieldInfo) -> Option<(String, bool)> {
    if f.field.is_empty() {
        if f.value.is_empty() {
            return None;
        }
        let phrase = format!("\"{}\"", f.value.replace('\\', "\\\\").replace('"', "\\\""));
        return Some((
            format!(r#"{{"query_string":{{"lenient":true,"query":{}}}}}"#, go_json_str(&phrase)),
            f.op == "!:",
        ));
    }
    let exists = format!(r#"{{"exists":{{"field":{}}}}}"#, go_json_str(&f.field));
    let field = go_json_str(&exact_field(&f.field, info));
    let value = go_json_str(&f.value);
    let term = format!(r#"{{"term":{{{field}:{value}}}}}"#);
    match f.op.as_str() {
        "null" => return Some((exists, true)),
        "notnull" => return Some((exists, false)),
        "=" | "!=" => return Some((term, f.op == "!=")),
        ":" => {
            if f.value.is_empty() {
                return Some((exists, false));
            }
            // wildcard 는 글자 필드에만 된다. 숫자·날짜 같은 필드는 같음으로 본다
            let (typ, _) = info(&f.field);
            if !typ.is_empty() && typ != "text" && typ != "keyword" && typ != "wildcard" {
                return Some((term, false));
            }
            let mut pat = String::from("*");
            for c in f.value.chars() {
                if matches!(c, '\\' | '*' | '?') {
                    pat.push('\\');
                }
                pat.push(c);
            }
            pat.push('*');
            return Some((
                format!(
                    r#"{{"wildcard":{{{field}:{{"case_insensitive":true,"value":{}}}}}}}"#,
                    go_json_str(&pat)
                ),
                false,
            ));
        }
        _ => {}
    }
    let op = match f.op.as_str() {
        ">" => "gt",
        ">=" => "gte",
        "<" => "lt",
        "<=" => "lte",
        _ => "",
    };
    let numeric = f.value.trim_matches(|c: char| "0123456789+-.eE".contains(c)).is_empty()
        && serde_json::from_str::<serde_json::Number>(&f.value).is_ok();
    let v = if numeric { f.value.clone() } else { value };
    Some((format!(r#"{{"range":{{{field}:{{{}:{v}}}}}}}"#, go_json_str(op)), false))
}

// ─── 결과 ───

/// _search 응답을 `_index`·`_id`·`_score` + _source 최상위 필드 합집합(처음 본 순서) 표로 만든다.
/// 집계가 있으면 `aggregations` 열과 그 JSON 을 담은 마지막 행을 더한다. 문서가 없으면 집계 열 하나짜리 표다.
fn search_result(data: &[u8], limit: usize) -> Result<QueryResult, Error> {
    #[derive(Deserialize)]
    struct Hit {
        #[serde(default, rename = "_index", deserialize_with = "raw_some")]
        index: Option<Box<RawValue>>,
        #[serde(default, rename = "_id", deserialize_with = "raw_some")]
        id: Option<Box<RawValue>>,
        #[serde(default, rename = "_score", deserialize_with = "raw_some")]
        score: Option<Box<RawValue>>,
        #[serde(default, rename = "_source", deserialize_with = "raw_some")]
        source: Option<Box<RawValue>>,
    }
    #[derive(Deserialize, Default)]
    struct Hits {
        #[serde(default)]
        hits: Option<Vec<Hit>>,
    }
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default, deserialize_with = "null_default")]
        hits: Hits,
        #[serde(default, deserialize_with = "raw_some")]
        aggregations: Option<Box<RawValue>>,
    }
    let r: Resp = serde_json::from_slice(data).map_err(|e| Error::msg(format!("opensearch: decode _search: {}", json_error(&e))))?;
    let mut hits = r.hits.hits.unwrap_or_default();
    let aggs = r.aggregations.as_ref().map(|a| cell(a.get()));
    if hits.is_empty()
        && let Some(a) = aggs
    {
        return Ok(QueryResult {
            cols: vec!["aggregations".into()],
            rows: vec![vec![a]],
            ..Default::default()
        });
    }
    let truncated = hits.len() > limit;
    hits.truncate(limit);
    let sources: Vec<&str> = hits.iter().map(|h| raw_text(&h.source)).collect();
    let mut res = object_table(&["_index", "_id", "_score"], &sources, limit)?;
    res.truncated = truncated;
    for (row, h) in res.rows.iter_mut().zip(&hits) {
        row[0] = cell(raw_text(&h.index));
        row[1] = cell(raw_text(&h.id));
        row[2] = cell(raw_text(&h.score));
    }
    if let Some(a) = aggs {
        res.cols.push("aggregations".into());
        for row in &mut res.rows {
            row.push(NULL_TEXT.into());
        }
        let mut last = vec![NULL_TEXT.to_string(); res.cols.len()];
        *last.last_mut().unwrap_or(&mut String::new()) = a;
        res.rows.push(last);
    }
    Ok(res)
}

/// JSON 객체 목록을 lead 열 + 최상위 키 합집합(처음 본 순서) 표로 만든다.
/// lead 열 칸과 없는 키는 "NULL" 로 두고, limit 을 넘는 행은 버리고 truncated 로 알린다.
fn object_table(lead: &[&str], objs: &[&str], limit: usize) -> Result<QueryResult, Error> {
    let mut res = QueryResult {
        cols: lead.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let objs = if objs.len() > limit {
        res.truncated = true;
        &objs[..limit]
    } else {
        objs
    };
    // _source 에 같은 이름(_id 등)이 있어도 열을 겹쳐 만들지 않는다
    let mut pos: HashMap<String, usize> = lead.iter().enumerate().map(|(i, c)| (c.to_string(), i)).collect();
    let mut decoded = Vec::with_capacity(objs.len());
    for o in objs {
        let mut m: HashMap<String, Box<RawValue>> = HashMap::new();
        for (key, v) in ordered_object(o)? {
            if !pos.contains_key(&key) {
                pos.insert(key.clone(), res.cols.len());
                res.cols.push(key.clone());
            }
            m.insert(key, v);
        }
        decoded.push(m);
    }
    for m in decoded {
        let row = res
            .cols
            .iter()
            .enumerate()
            .map(|(c, col)| match m.get(col) {
                Some(v) if c >= lead.len() => cell(v.get()),
                _ => NULL_TEXT.to_string(),
            })
            .collect();
        res.rows.push(row);
    }
    Ok(res)
}

/// JSON 객체의 키를 글에 나온 순서대로 돌려준다(map 으로 풀면 순서를 잃는다). null·빈 값은 키 없음이다.
fn ordered_object(raw: &str) -> Result<Vec<(String, Box<RawValue>)>, Error> {
    let t = raw.trim();
    if t.is_empty() || t == "null" {
        return Ok(Vec::new());
    }
    if !t.starts_with('{') {
        let head: String = raw.chars().take(40).collect();
        return Err(Error::msg(format!("opensearch: expected JSON object, got {head}")));
    }
    serde_json::from_str::<Ordered>(t)
        .map(|o| o.0)
        .map_err(|e| Error::msg(format!("opensearch: decode object: {}", json_error(&e))))
}

struct Ordered(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Ordered;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, Box<RawValue>>()? {
                    out.push((k, v));
                }
                Ok(Ordered(out))
            }
        }
        d.deserialize_map(V)
    }
}

/// JSON 값을 표 칸 글로 바꾼다. null·없음은 "NULL", 문자열은 따옴표 없이,
/// 숫자·불은 원문, 객체·배열은 압축 JSON(값 보기 R32 로 연다)이다. MAX_CELL_BYTES 를 넘으면 자른다.
fn cell(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() || raw == "null" {
        return NULL_TEXT.into();
    }
    let s = match raw.as_bytes()[0] {
        b'"' => serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_string()),
        b'{' | b'[' => compact(raw, false),
        _ => raw.to_string(),
    };
    cap_cell(s.as_bytes())
}

fn raw_text(v: &Option<Box<RawValue>>) -> &str {
    v.as_ref().map_or("", |r| r.get())
}

/// Go `json.Compact`(escape_html 이면 json.Marshal 이 RawMessage 에 쓰는 꼴)다. 문자열 밖 공백만 지운다.
fn compact(raw: &str, escape_html: bool) -> String {
    let mut out = String::with_capacity(raw.len());
    let (mut in_str, mut esc) = (false, false);
    for c in raw.chars() {
        if in_str {
            match c {
                _ if esc => esc = false,
                '\\' => esc = true,
                '"' => in_str = false,
                _ => {}
            }
        } else if c == '"' {
            in_str = true;
        } else if matches!(c, ' ' | '\t' | '\n' | '\r') {
            continue;
        }
        match c {
            '<' | '>' | '&' if escape_html => out.push_str(&format!("\\u{:04x}", c as u32)),
            '\u{2028}' | '\u{2029}' if escape_html => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Go json.Marshal 의 문자열(HTML 글자 이스케이프 포함)이다.
fn go_json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    write_str(&mut out, s);
    out.replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026")
}

// ─── serde 도움 ───

fn decode<T: for<'de> Deserialize<'de>>(data: &[u8], what: &str) -> Result<T, Error> {
    serde_json::from_slice(data).map_err(|e| Error::msg(format!("opensearch: decode {what}: {}", json_error(&e))))
}

/// null 을 빈 글로 받는다(Go 는 null 을 건너뛰어 빈 값으로 둔다).
pub(crate) fn nullable<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Option::<String>::deserialize(d).map(Option::unwrap_or_default)
}

pub(crate) fn null_default<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(d: D) -> Result<T, D::Error> {
    Option::<T>::deserialize(d).map(Option::unwrap_or_default)
}

/// 키가 있으면 null 이라도 원문으로 받는다(Go json.RawMessage 는 null 도 "null" 로 담는다).
fn raw_some<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Box<RawValue>>, D::Error> {
    Box::<RawValue>::deserialize(d).map(Some)
}

#[cfg(test)]
mod tests;
