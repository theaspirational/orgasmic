//! Authorized incremental delivery of persisted session envelopes.
use crate::{
    api::{authorize_run_read, get_run, ApiState},
    authz::Identity,
    events::EventPayload,
};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Extension, Path as RoutePath, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures::{SinkExt, StreamExt};
use orgasmic_core::SessionEnvelope;
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub async fn handler(
    ws: WebSocketUpgrade,
    RoutePath(run_id): RoutePath<String>,
    Extension(identity): Extension<Identity>,
    State(state): State<ApiState>,
) -> Response {
    let detail = match get_run(State(state.clone()), RoutePath(run_id.clone())).await {
        Ok(detail) => detail.0,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = authorize_run_read(&identity, &detail["run"]) {
        return error.into_response();
    }
    let Some(path) = detail["run"]["session_path"].as_str().map(PathBuf::from) else {
        return (StatusCode::NOT_FOUND, "run session path is unavailable").into_response();
    };
    let events = state.events.subscribe();
    ws.on_upgrade(move |socket| serve(socket, path, run_id, events, state.shutdown))
}

#[derive(Default)]
struct Tail {
    offset: u64,
    partial: Vec<u8>,
    #[cfg(unix)]
    identity: Option<(u64, u64)>,
}
impl Tail {
    async fn poll(&mut self, path: &Path) -> anyhow::Result<(bool, Vec<SessionEnvelope>)> {
        let mut file = tokio::fs::File::open(path).await?;
        let metadata = file.metadata().await?;
        let mut reset = metadata.len() < self.offset;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let identity = (metadata.dev(), metadata.ino());
            reset |= self.identity.is_some_and(|old| old != identity);
            self.identity = Some(identity);
        }
        if reset {
            self.offset = 0;
            self.partial.clear();
        }
        file.seek(std::io::SeekFrom::Start(self.offset)).await?;
        let mut bytes = self.partial.clone();
        let read = file.read_to_end(&mut bytes).await?;
        let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        let envelopes = bytes[..end]
            .split(|b| *b == b'\n')
            .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
            .map(serde_json::from_slice)
            .collect::<Result<Vec<SessionEnvelope>, _>>()?;
        self.offset += read as u64;
        self.partial = bytes[end..].to_vec();
        Ok((reset, envelopes))
    }
}

async fn serve(
    socket: WebSocket,
    path: PathBuf,
    run_id: String,
    mut events: tokio::sync::broadcast::Receiver<crate::events::Event>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let (mut sender, mut receiver) = socket.split();
    let mut tail = Tail::default();
    let mut first = true;
    let mut delivery_seq = 0u64;
    // Recover bus lag as well as a write whose final newline arrived after its notification.
    let mut reconcile = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        match tail.poll(&path).await {
            Ok((reset, envelopes)) => {
                let replay = first || reset;
                if replay {
                    delivery_seq = 0;
                }
                // Persisted seq restarts when a SessionWriter reopens. Delivery order
                // is the file order, independent of those producer-local sequences.
                let envelopes: Vec<_> = envelopes
                    .into_iter()
                    .map(|envelope| {
                        let mut value =
                            serde_json::to_value(envelope).expect("session envelope serializes");
                        value["delivery_seq"] = json!(delivery_seq);
                        delivery_seq += 1;
                        value
                    })
                    .collect();
                let mut batches = envelopes.chunks(128).peekable();
                if replay
                    && batches.peek().is_none()
                    && sender
                        .send(Message::Text(
                            json!({"type":"snapshot","envelopes":[]}).to_string(),
                        ))
                        .await
                        .is_err()
                {
                    return;
                }
                for (index, batch) in batches.enumerate() {
                    let kind = if replay && index == 0 {
                        "snapshot"
                    } else {
                        "append"
                    };
                    if sender
                        .send(Message::Text(
                            json!({"type":kind,"envelopes":batch,"replay":replay}).to_string(),
                        ))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                first = false;
            }
            Err(error) => {
                let _ = sender.send(Message::Text(json!({"type":"error","message":format!("Cannot read transcript: {error}")}).to_string())).await;
                break;
            }
        }
        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                message = receiver.next() => match message {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                    Some(Ok(Message::Ping(data))) => { if sender.send(Message::Pong(data)).await.is_err() { return; } },
                    _ => {},
                },
                _ = reconcile.tick() => break,
                event = events.recv() => match event {
                    Ok(event) if matches!(event.payload, EventPayload::RunEvent { run_id: ref id, .. } if id == &run_id) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    _ => {},
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    #[tokio::test]
    async fn transcript_tail_reads_new_complete_lines_and_resets_on_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        let line = |seq| {
            format!("{{\"seq\":{seq},\"time\":\"2026-09-07T10:00:00Z\",\"run_id\":\"r\",\"runtime_id\":\"x\",\"boot_id\":\"b\",\"kind\":\"driver_event\",\"event\":{{\"type\":\"text_chunk\",\"chunk\":\"héllo\"}}}}\n")
        };
        tokio::fs::write(&path, line(0)).await.unwrap();
        let mut tail = Tail::default();
        assert_eq!(tail.poll(&path).await.unwrap().1.len(), 1);
        assert!(tail.poll(&path).await.unwrap().1.is_empty());
        let second = line(1);
        let split = second.find('é').unwrap() + 1;
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        file.write_all(&second.as_bytes()[..split]).await.unwrap();
        assert!(tail.poll(&path).await.unwrap().1.is_empty());
        file.write_all(&second.as_bytes()[split..]).await.unwrap();
        assert_eq!(tail.poll(&path).await.unwrap().1[0].seq, 1);
        tokio::fs::write(dir.path().join("replacement"), line(0))
            .await
            .unwrap();
        tokio::fs::rename(dir.path().join("replacement"), &path)
            .await
            .unwrap();
        let (reset, events) = tail.poll(&path).await.unwrap();
        assert!(reset);
        assert_eq!(events[0].seq, 0);
    }
}
