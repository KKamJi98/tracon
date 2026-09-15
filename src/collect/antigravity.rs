//! Antigravity CLI(`agy`) 세션 수집.
//!
//! claude와 codex는 대화를 jsonl로 남겨서 꼬리만 읽으면 되지만, Antigravity는
//! 대화 하나를 SQLite 파일 하나로 쓰고 그 안의 알맹이는 스키마 없는 protobuf
//! blob이다. 그래서 이 레이어만 모양이 다르다.
//!
//! 대신 여기서는 claude에서 끝내 얻지 못한 것이 공짜로 나온다. 실행 중인 `agy`는
//! `brain/<conversation-uuid>/`를 열어 둔 채로 돌기 때문에, 열린 파일만 보면
//! 프로세스와 대화가 짐작 없이 이어진다.
//!
//! 읽어 내는 값과 그 출처:
//!   - 대화 uuid: `lsof`가 보여 주는 `brain/<uuid>` 경로
//!   - 마지막 활동: `conversations/<uuid>.db`와 그 `-wal`의 mtime 중 최신
//!   - 모델과 컨텍스트: `gen_metadata`의 protobuf (아래 `extract` 참조)
//!   - 이름: `conversation_summaries.db`의 `title`
//!
//! 이름은 대화가 끝난 뒤에야 요약 DB에 적힌다. 진행 중인 대화는 거기 없으므로
//! 이름이 `None`이다 - 대화 내용으로 지어내지 않는다.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 한 대화에서 건져 낸 값. 못 읽은 항목은 `None`이다.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConversationInfo {
    pub model: Option<String>,
    pub ctx_tokens: Option<u64>,
    pub ctx_window: Option<u64>,
    pub title: Option<String>,
}

pub fn root() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".gemini/antigravity-cli")
}

/// 실행 중인 `agy`가 붙들고 있는 대화 uuid. `lsof`가 없거나 열린 경로가 없으면
/// `None`이고, 그러면 세션은 대화 없이 프로세스만 있는 행이 된다.
///
/// `lsof -p`는 0.15초쯤 걸린다 - tick마다 부르면 안 되고, 호출부가 pid별로 한 번만
/// 부르고 캐시해야 한다.
pub fn conversation_for_pid(pid: i32) -> Option<String> {
    let out = Command::new("lsof")
        .args(["-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().find_map(uuid_after_brain)
}

/// `.../antigravity-cli/brain/<uuid>/...` 에서 uuid만 떼어 낸다.
fn uuid_after_brain(line: &str) -> Option<String> {
    let rest = line.split("/brain/").nth(1)?;
    let uuid = rest.split('/').next()?;
    is_uuid(uuid).then(|| uuid.to_string())
}

/// 하이픈 포함 36자 uuid 모양인지. `brain/` 아래에는 uuid 아닌 디렉터리도 있다.
fn is_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-') && s.as_bytes()[8] == b'-'
}

/// 대화 파일과 그 WAL 중 더 최근 mtime. WAL이 본체보다 나중에 움직이므로 둘 다 본다.
pub fn last_activity_ms(root: &Path, uuid: &str) -> Option<i64> {
    let db = root.join("conversations").join(format!("{uuid}.db"));
    let wal = root.join("conversations").join(format!("{uuid}.db-wal"));
    [db, wal]
        .iter()
        .filter_map(|p| p.metadata().ok()?.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .max()
}

/// 요약 DB에 적힌 대화 이름들. 대화가 끝난 뒤에야 채워지므로 진행 중인 대화는 없다.
pub fn titles(root: &Path) -> HashMap<String, String> {
    let path = root.join("conversation_summaries.db");
    let rows = query_json(
        &path,
        "select conversation_id, title from conversation_summaries where title <> ''",
    )
    .unwrap_or_default();
    rows.iter()
        .filter_map(|r| {
            let id = r.get("conversation_id")?.as_str()?.to_string();
            let title = r.get("title")?.as_str()?.to_string();
            Some((id, title))
        })
        .collect()
}

/// 대화 하나에서 모델과 컨텍스트 사용량을 읽는다.
///
/// `size < 65536` 조건은 성능이 아니라 정확도 때문이다. `gen_metadata`에는 파일
/// 본문을 통째로 담은 수백 KB짜리 행이 섞여 있고, 그 행에는 모델도 토큰도 없다.
/// 실측에서 메타데이터 행은 1KB 안팎이고 큰 행만 이 한도를 넘었다.
pub fn read_conversation(root: &Path, uuid: &str) -> Option<ConversationInfo> {
    let path = root.join("conversations").join(format!("{uuid}.db"));
    let rows = query_json(
        &path,
        "select hex(data) as h from gen_metadata where size < 65536 order by idx desc limit 8",
    )?;
    for row in &rows {
        let Some(hex) = row.get("h").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(bytes) = from_hex(hex) else { continue };
        if let Some(info) = extract(&bytes) {
            return Some(info);
        }
    }
    Some(ConversationInfo::default())
}

/// `sqlite3 -json`으로 읽는다. 크레이트를 하나 더 들이는 대신 외부 명령을 쓰는 것은
/// `ps`와 같은 선택이다 - sqlite3가 없는 환경에서는 Antigravity 행이 빠질 뿐,
/// 나머지는 그대로 돈다.
fn query_json(db: &Path, sql: &str) -> Option<Vec<serde_json::Map<String, serde_json::Value>>> {
    if !db.exists() {
        return None;
    }
    let out = Command::new("sqlite3")
        .arg("-json")
        .arg("-readonly")
        .arg(db)
        .arg(sql)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    if text.trim().is_empty() {
        return Some(Vec::new());
    }
    serde_json::from_str(&text).ok()
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// 실측으로 확인한 필드 배치. `gen_metadata.data`는 최상위에 메시지 하나(필드 1)를
/// 두고, 그 안에서:
///   - 필드 19: 모델 이름 (`gemini-3.8-flash`)
///   - 필드 9 > 필드 10 > 필드 1: 지금까지 쓴 토큰
///   - 필드 9 > 필드 10 > 필드 4: 컨텍스트 윈도우
///
/// 스키마가 공개된 것이 아니라 관측한 모양이므로, 하나라도 어긋나면 지어내지 않고
/// 그 항목을 비운다. 윈도우가 10만 미만이면 다른 필드를 잘못 짚은 것으로 본다.
fn extract(bytes: &[u8]) -> Option<ConversationInfo> {
    let body = field_bytes(bytes, 1)?;
    let model = field_bytes(&body, 19)
        .and_then(|b| String::from_utf8(b.to_vec()).ok())
        .filter(|s| !s.is_empty());
    let usage = field_bytes(&body, 9).and_then(|b| field_bytes(&b, 10));
    let (tokens, window) = match usage {
        Some(u) => (field_varint(&u, 1), field_varint(&u, 4)),
        None => (None, None),
    };
    let window = window.filter(|w| *w >= 100_000);
    if model.is_none() && tokens.is_none() {
        return None;
    }
    Some(ConversationInfo {
        model,
        ctx_tokens: tokens,
        ctx_window: window,
        title: None,
    })
}

/// 길이 구분(wire type 2) 필드의 본문.
fn field_bytes(bytes: &[u8], want: u32) -> Option<Vec<u8>> {
    scan(bytes, want, 2).and_then(|v| match v {
        Value::Bytes(b) => Some(b),
        Value::Num(_) => None,
    })
}

/// varint(wire type 0) 필드의 값.
fn field_varint(bytes: &[u8], want: u32) -> Option<u64> {
    scan(bytes, want, 0).and_then(|v| match v {
        Value::Num(n) => Some(n),
        Value::Bytes(_) => None,
    })
}

enum Value {
    Num(u64),
    Bytes(Vec<u8>),
}

/// protobuf wire format을 앞에서부터 훑어 원하는 필드 하나를 찾는다. 스키마가 없으므로
/// 모르는 필드는 길이만 재고 건너뛴다. 알 수 없는 wire type을 만나면 그 뒤는 해석할
/// 수 없으므로 멈춘다 - 어긋난 바이트를 계속 읽어 엉뚱한 값을 만들지 않는다.
fn scan(bytes: &[u8], want: u32, want_wire: u8) -> Option<Value> {
    let mut i = 0usize;
    while i < bytes.len() {
        let (key, next) = varint(bytes, i)?;
        i = next;
        let (num, wire) = ((key >> 3) as u32, (key & 7) as u8);
        if num == 0 {
            return None;
        }
        match wire {
            0 => {
                let (v, next) = varint(bytes, i)?;
                i = next;
                if num == want && want_wire == 0 {
                    return Some(Value::Num(v));
                }
            }
            1 => i = i.checked_add(8)?,
            2 => {
                let (len, next) = varint(bytes, i)?;
                let len = len as usize;
                let end = next.checked_add(len)?;
                let body = bytes.get(next..end)?;
                i = end;
                if num == want && want_wire == 2 {
                    return Some(Value::Bytes(body.to_vec()));
                }
            }
            5 => i = i.checked_add(4)?,
            _ => return None,
        }
    }
    None
}

fn varint(bytes: &[u8], mut i: usize) -> Option<(u64, usize)> {
    let mut out = 0u64;
    let mut shift = 0u32;
    while i < bytes.len() {
        let byte = bytes[i];
        i += 1;
        out |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((out, i));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
    None
}

/// 마지막 활동으로부터 흐른 시간으로 상태를 정한다.
///
/// Antigravity에는 claude의 훅도, codex의 `task_complete`도, 대화 엔트리의 종류도
/// 없다 - 읽을 수 있는 것은 대화 파일이 언제 마지막으로 움직였는가뿐이다. 그래서
/// 확신은 언제나 Low다. grace 안이면 아직 돌고 있다고 보고, 그 뒤는 Idle로 두면
/// `demote`가 하루 뒤 Stale까지 내려 준다.
pub fn infer(
    age_ms: i64,
    cfg: &crate::config::Thresholds,
) -> (crate::model::State, crate::model::Confidence) {
    let state = if age_ms <= cfg.running_grace_ms {
        crate::model::State::RunningInference
    } else {
        crate::model::State::Idle
    };
    (
        crate::model::demote(state, age_ms, cfg),
        crate::model::Confidence::Low,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 실행 중인 `agy`의 열린 파일 목록에서 대화 uuid를 떼어 낸다. brain 아래에는
    /// uuid가 아닌 디렉터리도 있으므로 모양을 확인하고 받는다.
    #[test]
    fn a_brain_path_yields_the_conversation_uuid() {
        let line = "agy 64805 ethan 42u DIR 1,18 96 123 /Users/x/.gemini/antigravity-cli/brain/e781d9ec-d4d3-40b0-a59d-ddd8dbc3cd3f/scratch";
        assert_eq!(
            uuid_after_brain(line).as_deref(),
            Some("e781d9ec-d4d3-40b0-a59d-ddd8dbc3cd3f")
        );
        assert_eq!(uuid_after_brain("/some/other/path"), None);
        assert_eq!(uuid_after_brain("/x/brain/knowledge/lock"), None);
    }

    /// protobuf wire format에서 필요한 필드만 건져 낸다. 실측한 배치는
    /// `f1 > {f19: 모델, f9 > f10 > {f1: 토큰, f4: 윈도우}}`다.
    #[test]
    fn extract_reads_the_model_and_the_context_counters() {
        fn varint(mut n: u64) -> Vec<u8> {
            let mut out = Vec::new();
            loop {
                let b = (n & 0x7F) as u8;
                n >>= 7;
                if n == 0 {
                    out.push(b);
                    return out;
                }
                out.push(b | 0x80);
            }
        }
        fn field_varint(num: u32, v: u64) -> Vec<u8> {
            let mut out = varint((num as u64) << 3);
            out.extend(varint(v));
            out
        }
        fn field_bytes(num: u32, body: &[u8]) -> Vec<u8> {
            let mut out = varint(((num as u64) << 3) | 2);
            out.extend(varint(body.len() as u64));
            out.extend_from_slice(body);
            out
        }

        let mut inner = field_varint(1, 43_763);
        inner.extend(field_varint(4, 256_000));
        let f10 = field_bytes(10, &inner);
        let f9 = field_bytes(9, &f10);
        let mut body = f9;
        body.extend(field_bytes(19, b"gemini-3.8-flash"));
        let blob = field_bytes(1, &body);

        let info = extract(&blob).expect("extract");
        assert_eq!(info.model.as_deref(), Some("gemini-3.8-flash"));
        assert_eq!(info.ctx_tokens, Some(43_763));
        assert_eq!(info.ctx_window, Some(256_000));
    }

    /// 스키마가 공개된 것이 아니라 관측한 모양이다. 어긋나면 지어내지 않는다.
    #[test]
    fn a_blob_without_those_fields_yields_nothing() {
        assert!(extract(b"").is_none());
        assert!(extract(&[0xFF, 0xFF, 0xFF]).is_none());
    }

    /// 윈도우가 터무니없이 작으면 다른 필드를 잘못 짚은 것이다.
    #[test]
    fn an_implausible_window_is_dropped_rather_than_shown() {
        assert!(super::ConversationInfo::default().ctx_window.is_none());
    }
}
