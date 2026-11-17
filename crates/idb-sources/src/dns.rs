//! Redis TLS 의 서버 이름에 쓸 CNAME 대상을 찾는다(Go `net.LookupCNAME`, 명세 R4.3).
//! ElastiCache 인증서는 *.cache.amazonaws.com 이라 사내 .local 별칭으로는 검증이 맞지 않아 별칭이 가리키는 이름을 쓴다.
//! 표준 라이브러리에 CNAME 조회가 없어 /etc/resolv.conf 의 첫 이름 서버에 A 질의를 한 번 보내 답의 CNAME 사슬 끝을 읽는다.
//! 한계: search 도메인 덧붙이기·TCP 재질의·/etc/hosts 는 보지 않는다. 실패하면 호출부가 호스트 이름 그대로 검증한다.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::net::UdpSocket;

/// host 의 정규 이름(끝 점 없음)이다. 못 찾으면 None 이다.
pub(crate) async fn lookup_cname(host: &str) -> Option<String> {
    if host.parse::<IpAddr>().is_ok() {
        return None;
    }
    let server = nameserver()?;
    let sock = UdpSocket::bind(if server.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).await.ok()?;
    sock.connect(server).await.ok()?;
    let id = (std::process::id() as u16) ^ 0x5a5a;
    sock.send(&query(id, host)?).await.ok()?;
    let mut buf = vec![0u8; 1500];
    let n = tokio::time::timeout(Duration::from_secs(3), sock.recv(&mut buf)).await.ok()?.ok()?;
    canonical(&buf[..n], id, host)
}

fn nameserver() -> Option<SocketAddr> {
    let conf = std::fs::read_to_string("/etc/resolv.conf").ok()?;
    conf.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        (f.next()? == "nameserver")
            .then(|| f.next())?
            .and_then(|a| a.parse::<IpAddr>().ok())
            .map(|ip| SocketAddr::new(ip, 53))
    })
}

/// 재귀를 바라는 A 질의 하나다.
fn query(id: u16, host: &str) -> Option<Vec<u8>> {
    let mut q = Vec::with_capacity(32 + host.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in host.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&[0, 1, 0, 1]);
    Some(q)
}

/// 답에서 host 부터 CNAME 을 따라간 끝 이름이다. CNAME 이 없으면 host 다.
fn canonical(msg: &[u8], id: u16, host: &str) -> Option<String> {
    if msg.len() < 12 || msg[..2] != id.to_be_bytes() || msg[3] & 0x0f != 0 {
        return None;
    }
    let qd = u16::from_be_bytes([msg[4], msg[5]]);
    let an = u16::from_be_bytes([msg[6], msg[7]]);
    let mut pos = 12;
    for _ in 0..qd {
        pos = read_name(msg, pos)?.1 + 4;
    }
    let mut cnames = Vec::new();
    for _ in 0..an {
        let (name, p) = read_name(msg, pos)?;
        let typ = u16::from_be_bytes([*msg.get(p)?, *msg.get(p + 1)?]);
        let len = u16::from_be_bytes([*msg.get(p + 8)?, *msg.get(p + 9)?]) as usize;
        let data = p + 10;
        if typ == 5 {
            cnames.push((name.to_ascii_lowercase(), read_name(msg, data)?.0));
        }
        pos = data + len;
    }
    let mut cur = host.trim_end_matches('.').to_string();
    // 사슬이 고리여도 끝나게 답의 수만큼만 따라간다
    for _ in 0..=cnames.len() {
        match cnames.iter().find(|(n, _)| *n == cur.to_ascii_lowercase()) {
            Some((_, target)) => cur = target.clone(),
            None => break,
        }
    }
    Some(cur)
}

/// pos 의 이름(압축 포함)과 그 뒤 자리다.
fn read_name(msg: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut end = None;
    for _ in 0..128 {
        let len = *msg.get(pos)? as usize;
        if len & 0xc0 == 0xc0 {
            let ptr = ((len & 0x3f) << 8) | *msg.get(pos + 1)? as usize;
            end.get_or_insert(pos + 2);
            pos = ptr;
            continue;
        }
        if len == 0 {
            return Some((labels.join("."), end.unwrap_or(pos + 1)));
        }
        labels.push(String::from_utf8_lossy(msg.get(pos + 1..pos + 1 + len)?).into_owned());
        pos += 1 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 답의_CNAME_사슬_끝을_정규_이름으로_읽는다() {
        let id = 7;
        let mut m = query(id, "r.local").unwrap();
        m[2] = 0x81;
        m[3] = 0x80;
        m[7] = 2; // 답 2개
        // r.local CNAME c.cache.amazonaws.com (질문 이름은 12 자리라 압축 0xc00c)
        m.extend_from_slice(&[0xc0, 12, 0, 5, 0, 1, 0, 0, 0, 60]);
        let target = [b"\x01c\x05cache\x09amazonaws\x03com\x00".as_slice()].concat();
        m.extend_from_slice(&(target.len() as u16).to_be_bytes());
        let target_at = m.len();
        m.extend_from_slice(&target);
        // c.cache.amazonaws.com A 10.0.0.1
        m.extend_from_slice(&[0xc0, target_at as u8, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 10, 0, 0, 1]);
        assert_eq!(canonical(&m, id, "r.local").as_deref(), Some("c.cache.amazonaws.com"));
        assert_eq!(canonical(&m, id + 1, "r.local"), None);
    }

    #[test]
    fn CNAME_이_없으면_이름_그대로다() {
        let mut m = query(9, "plain.example").unwrap();
        m[2] = 0x81;
        m[3] = 0x80;
        assert_eq!(canonical(&m, 9, "plain.example.").as_deref(), Some("plain.example"));
    }
}
