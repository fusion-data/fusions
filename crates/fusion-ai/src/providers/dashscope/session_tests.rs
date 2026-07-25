//! 会话级测试:对着一个**假 DashScope 服务端**跑完整的 `transcribe_realtime` 事件循环。
//!
//! 为什么必须有这一层:纯函数测试(协议解析、地域校验、序列化)覆盖不到 `try_stream!` 主体,
//! 而握手时序、ping 容忍、上行泵分发、finish-task 时序、时长累计、提前取消的 abort 语义
//! 全在那里 —— 也正是历史上出过静默丢结果、ping 打死会话、成功后仍报可重试错误的地方。
//!
//! 假服务端说 `ws://`,故通过 `DashScopeRegion` 之外的注入点接入:测试用
//! [`FunAsrRealtime::with_endpoint_for_test`] 覆盖 endpoint。

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::protocol::Message;

use super::fun_asr::FunAsrRealtime;
use crate::providers::dashscope::DashScopeCredentials;
use crate::speech_to_text::{AudioStreamConfig, SpeechToText, SpeechToTextError, SttEvent, SttUplink, SttUplinkStream};

/// 服务端脚本:收到什么就回什么。
type Script = Arc<dyn Fn(ServerHandle) -> tokio::task::JoinHandle<()> + Send + Sync>;

/// 假服务端暴露给脚本的句柄。
pub struct ServerHandle {
  pub ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
  /// 服务端收到的上行帧(供断言)。
  pub received: Arc<Mutex<Vec<Message>>>,
}

/// 启动假服务端,返回 (ws_url, 收到的帧, 服务端任务句柄)。
async fn spawn_server(script: Script) -> (String, Arc<Mutex<Vec<Message>>>, tokio::task::JoinHandle<()>) {
  let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
  let addr = listener.local_addr().expect("addr");
  let received = Arc::new(Mutex::new(Vec::new()));
  let received_for_task = received.clone();
  let handle = tokio::spawn(async move {
    let Ok((stream, _)) = listener.accept().await else { return };
    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else { return };
    let inner = script(ServerHandle { ws, received: received_for_task });
    let _ = inner.await;
  });
  (format!("ws://{addr}"), received, handle)
}

fn provider(endpoint: &str) -> FunAsrRealtime {
  FunAsrRealtime::new(DashScopeCredentials { api_key: "test-key".into(), workspace_id: None })
    .with_endpoint_for_test(endpoint)
    .with_start_timeout(Duration::from_secs(3))
    .with_idle_timeout(Duration::from_secs(3))
}

fn cfg() -> AudioStreamConfig {
  AudioStreamConfig::pcm_s16le_16k_mono_40ms()
}

fn audio(frames: Vec<&'static [u8]>) -> SttUplinkStream {
  SttUplink::from_audio(futures::stream::iter(frames.into_iter().map(Bytes::from_static)))
}

fn started() -> Message {
  Message::Text(r#"{"header":{"task_id":"t","event":"task-started","attributes":{}},"payload":{}}"#.into())
}

fn result(text: &str, is_final: bool, duration: u64) -> Message {
  Message::Text(
    format!(
      r#"{{"header":{{"task_id":"t","event":"result-generated"}},"payload":{{"output":{{"sentence":{{"begin_time":0,"end_time":100,"text":"{text}","sentence_end":{is_final},"words":[]}}}},"usage":{{"duration":{duration}}}}}}}"#
    )
    .into(),
  )
}

fn finished(duration: Option<u64>) -> Message {
  let usage = duration.map_or("null".to_string(), |d| format!(r#"{{"duration":{d}}}"#));
  Message::Text(
    format!(r#"{{"header":{{"task_id":"t","event":"task-finished"}},"payload":{{"output":{{}},"usage":{usage}}}}}"#)
      .into(),
  )
}

/// 收集事件流直到结束,返回 (事件列表, 终态错误)。
async fn drain(mut events: crate::speech_to_text::SttEventStream) -> (Vec<SttEvent>, Option<SpeechToTextError>) {
  let mut out = Vec::new();
  while let Some(item) = events.next().await {
    match item {
      Ok(ev) => out.push(ev),
      Err(e) => return (out, Some(e)),
    }
  }
  (out, None)
}

// =========================================================================

#[tokio::test]
async fn happy_path_yields_started_partial_final_and_finished() {
  let script: Script = Arc::new(|h: ServerHandle| {
    tokio::spawn(async move {
      let (mut tx, mut rx) = h.ws.split();
      // run-task
      if let Some(Ok(m)) = rx.next().await {
        h.received.lock().await.push(m);
      }
      let _ = tx.send(started()).await;
      let _ = tx.send(result("张奶奶体温", false, 1)).await;
      let _ = tx.send(result("张奶奶体温三十八度", true, 3)).await;
      // 等 finish-task 再收尾
      while let Some(Ok(m)) = rx.next().await {
        let is_finish = matches!(&m, Message::Text(t) if t.contains("finish-task"));
        h.received.lock().await.push(m);
        if is_finish {
          break;
        }
      }
      let _ = tx.send(finished(Some(5))).await;
    })
  });
  let (url, received, server) = spawn_server(script).await;

  let events = provider(&url)
    .transcribe_realtime(audio(vec![b"aaaa", b"bbbb"]), cfg())
    .await
    .expect("stream opens");
  let (events, err) = drain(events).await;
  assert!(err.is_none(), "unexpected terminal error: {err:?}");

  assert!(matches!(events.first(), Some(SttEvent::Started { .. })));
  assert!(events.iter().any(|e| matches!(e, SttEvent::Partial(_))));
  assert!(events.iter().any(|e| matches!(e, SttEvent::SegmentFinal(_))));
  match events.last() {
    Some(SttEvent::TaskFinished(r)) => {
      assert_eq!(r.text, "张奶奶体温三十八度", "只有 final 段进最终文本");
      assert_eq!(r.audio_duration_ms, Some(5_000), "task-finished 的 usage 是权威值");
      assert_eq!(r.model, "fun-asr-realtime");
      assert_eq!(r.provider, "fun_asr_realtime");
    }
    other => panic!("expected TaskFinished last, got {other:?}"),
  }

  // 上行:run-task 先行,音频帧其后,finish-task 收尾。
  let frames = received.lock().await;
  let texts: Vec<String> = frames
    .iter()
    .filter_map(|m| if let Message::Text(t) = m { Some(t.to_string()) } else { None })
    .collect();
  assert!(texts.first().is_some_and(|t| t.contains("run-task")), "{texts:?}");
  assert!(texts.iter().any(|t| t.contains("finish-task")), "{texts:?}");
  drop(frames);
  server.abort();
}

#[tokio::test]
async fn a_ping_before_task_started_does_not_kill_the_session() {
  // 历史缺陷:等 task-started 只读一帧,服务端先发 ping 就以 Protocol 错误打死整次识别。
  let script: Script = Arc::new(|h: ServerHandle| {
    tokio::spawn(async move {
      let (mut tx, mut rx) = h.ws.split();
      let _ = rx.next().await; // run-task
      let _ = tx.send(Message::Ping(Vec::new().into())).await;
      let _ = tx.send(started()).await;
      let _ = tx.send(result("好", true, 1)).await;
      while let Some(Ok(m)) = rx.next().await {
        if matches!(&m, Message::Text(t) if t.contains("finish-task")) {
          break;
        }
      }
      let _ = tx.send(finished(Some(1))).await;
    })
  });
  let (url, _rx, server) = spawn_server(script).await;

  let events = provider(&url).transcribe_realtime(audio(vec![b"aa"]), cfg()).await.expect("stream opens");
  let (events, err) = drain(events).await;
  assert!(err.is_none(), "a ping must be ignorable, got {err:?}");
  assert!(matches!(events.first(), Some(SttEvent::Started { .. })));
  assert!(matches!(events.last(), Some(SttEvent::TaskFinished(_))));
  server.abort();
}

#[tokio::test]
async fn a_malformed_result_payload_fails_the_stream_instead_of_yielding_an_empty_transcript() {
  // 历史缺陷:payload 结构变化被当作可忽略帧 continue 掉 → 全部识别结果静默丢失,
  // 流仍以成功结束、text 为空。医疗录入通道里这是最坏的失败模式。
  let script: Script = Arc::new(|h: ServerHandle| {
    tokio::spawn(async move {
      let (mut tx, mut rx) = h.ws.split();
      let _ = rx.next().await;
      let _ = tx.send(started()).await;
      let _ = tx
        .send(Message::Text(
          r#"{"header":{"event":"result-generated"},"payload":{"output":{"sentence":{"text":42}}}}"#.into(),
        ))
        .await;
      let _ = tx.send(finished(Some(1))).await;
      // 保持连接,确认失败来自解析而非关闭。
      tokio::time::sleep(Duration::from_secs(2)).await;
    })
  });
  let (url, _rx, server) = spawn_server(script).await;

  let events = provider(&url).transcribe_realtime(audio(vec![b"aa"]), cfg()).await.expect("stream opens");
  let (events, err) = drain(events).await;
  assert!(matches!(err, Some(SpeechToTextError::Protocol(_))), "expected a protocol error, got {err:?}");
  assert!(
    !events.iter().any(|e| matches!(e, SttEvent::TaskFinished(_))),
    "MUST NOT report a successful empty transcript"
  );
  server.abort();
}

#[tokio::test]
async fn a_trailing_partial_is_kept_in_the_final_transcript() {
  // 服务端在 finish-task 后未 flush 终结版就收尾:末句已在 UI 上显示过,
  // 若不进最终文本,用户会以为它被收录了。
  let script: Script = Arc::new(|h: ServerHandle| {
    tokio::spawn(async move {
      let (mut tx, mut rx) = h.ws.split();
      let _ = rx.next().await;
      let _ = tx.send(started()).await;
      let _ = tx.send(result("血压一百二", true, 2)).await;
      let _ = tx.send(result("心率八十", false, 4)).await; // 未终结
      while let Some(Ok(m)) = rx.next().await {
        if matches!(&m, Message::Text(t) if t.contains("finish-task")) {
          break;
        }
      }
      let _ = tx.send(finished(Some(4))).await;
    })
  });
  let (url, _rx, server) = spawn_server(script).await;

  let events = provider(&url).transcribe_realtime(audio(vec![b"aa"]), cfg()).await.expect("stream opens");
  let (events, err) = drain(events).await;
  assert!(err.is_none(), "{err:?}");
  match events.last() {
    Some(SttEvent::TaskFinished(r)) => assert_eq!(r.text, "血压一百二心率八十", "末句未终结 partial 也要进最终文本"),
    other => panic!("expected TaskFinished, got {other:?}"),
  }
  server.abort();
}

#[tokio::test]
async fn task_failed_terminates_with_a_provider_error_carrying_retryability() {
  let script: Script = Arc::new(|h: ServerHandle| {
    tokio::spawn(async move {
      let (mut tx, mut rx) = h.ws.split();
      let _ = rx.next().await;
      let _ = tx.send(started()).await;
      let _ = tx
        .send(Message::Text(
          r#"{"header":{"event":"task-failed","error_code":"Throttling.RateQuota","error_message":"slow down"},"payload":{}}"#
            .into(),
        ))
        .await;
      tokio::time::sleep(Duration::from_secs(2)).await;
    })
  });
  let (url, _rx, server) = spawn_server(script).await;

  let events = provider(&url).transcribe_realtime(audio(vec![b"aa"]), cfg()).await.expect("stream opens");
  let (events, err) = drain(events).await;
  match err {
    Some(SpeechToTextError::Provider { code, retryable, .. }) => {
      assert_eq!(code, "Throttling.RateQuota");
      assert!(retryable);
    }
    other => panic!("expected a provider error, got {other:?}"),
  }
  // 同一次失败 MUST 只报一遍:事件流里不该再有第二份错误载体。
  assert!(events.iter().all(|e| !matches!(e, SttEvent::TaskFinished(_))));
  server.abort();
}

#[tokio::test]
async fn a_context_update_is_sent_as_continue_task_mid_session() {
  let script: Script = Arc::new(|h: ServerHandle| {
    tokio::spawn(async move {
      let (mut tx, mut rx) = h.ws.split();
      let _ = rx.next().await; // run-task
      let _ = tx.send(started()).await;
      while let Some(Ok(m)) = rx.next().await {
        let is_finish = matches!(&m, Message::Text(t) if t.contains("finish-task"));
        h.received.lock().await.push(m);
        if is_finish {
          break;
        }
      }
      let _ = tx.send(finished(Some(1))).await;
    })
  });
  let (url, received, server) = spawn_server(script).await;

  let uplink: SttUplinkStream = Box::pin(futures::stream::iter(vec![
    SttUplink::Audio(Bytes::from_static(b"aaaa")),
    SttUplink::ContextUpdate(vec!["利伐沙班".to_string()]),
    SttUplink::Audio(Bytes::from_static(b"bbbb")),
  ]));
  let events = provider(&url).transcribe_realtime(uplink, cfg()).await.expect("stream opens");
  let (_events, err) = drain(events).await;
  assert!(err.is_none(), "{err:?}");

  let frames = received.lock().await;
  let continue_task = frames
    .iter()
    .filter_map(|m| if let Message::Text(t) = m { Some(t.to_string()) } else { None })
    .find(|t| t.contains("continue-task"))
    .expect("a ContextUpdate must reach the provider as continue-task");
  assert!(continue_task.contains("利伐沙班"), "{continue_task}");
  drop(frames);
  server.abort();
}

#[tokio::test]
async fn an_empty_context_update_is_a_no_op_not_a_clear() {
  // 文档声明的语义:空列表 = 不变更(provider 无"清空上下文"的表达)。
  let script: Script = Arc::new(|h: ServerHandle| {
    tokio::spawn(async move {
      let (mut tx, mut rx) = h.ws.split();
      let _ = rx.next().await;
      let _ = tx.send(started()).await;
      while let Some(Ok(m)) = rx.next().await {
        let is_finish = matches!(&m, Message::Text(t) if t.contains("finish-task"));
        h.received.lock().await.push(m);
        if is_finish {
          break;
        }
      }
      let _ = tx.send(finished(None)).await;
    })
  });
  let (url, received, server) = spawn_server(script).await;

  let uplink: SttUplinkStream = Box::pin(futures::stream::iter(vec![SttUplink::ContextUpdate(vec!["  ".to_string()])]));
  let events = provider(&url).transcribe_realtime(uplink, cfg()).await.expect("stream opens");
  let (_events, err) = drain(events).await;
  assert!(err.is_none(), "{err:?}");

  let frames = received.lock().await;
  assert!(
    !frames.iter().any(|m| matches!(m, Message::Text(t) if t.contains("continue-task"))),
    "an all-blank context update must not hit the wire"
  );
  drop(frames);
  server.abort();
}

#[tokio::test]
async fn a_stalled_provider_times_out_instead_of_hanging_forever() {
  // 没有空闲上限时,provider 挂起会让事件流永久悬挂 —— 降级路径永远等不到触发点。
  let script: Script = Arc::new(|h: ServerHandle| {
    tokio::spawn(async move {
      let (mut tx, mut rx) = h.ws.split();
      let _ = rx.next().await;
      let _ = tx.send(started()).await;
      // 之后什么都不发,也不关连接。
      tokio::time::sleep(Duration::from_secs(30)).await;
    })
  });
  let (url, _rx, server) = spawn_server(script).await;

  let p = provider(&url).with_idle_timeout(Duration::from_millis(300));
  let events = p.transcribe_realtime(audio(vec![b"aa"]), cfg()).await.expect("stream opens");
  let (_events, err) = tokio::time::timeout(Duration::from_secs(5), drain(events))
    .await
    .expect("the stream must terminate on its own, not hang");
  assert!(matches!(err, Some(SpeechToTextError::Timeout(_))), "expected an idle timeout, got {err:?}");
  server.abort();
}

#[tokio::test]
async fn dropping_the_event_stream_aborts_the_uplink_pump() {
  // 消费方断连时,detach 的上行泵会一直持有 ws sink 并无限拉取(可能是活麦克风的)音频。
  let script: Script = Arc::new(|h: ServerHandle| {
    tokio::spawn(async move {
      let (mut tx, mut rx) = h.ws.split();
      let _ = rx.next().await;
      let _ = tx.send(started()).await;
      while rx.next().await.is_some() {}
    })
  });
  let (url, _rx, server) = spawn_server(script).await;

  // 无限音频流 + 一个观察计数器:泵被 abort 之后计数不再增长。
  let pulled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
  let pulled_for_stream = pulled.clone();
  let infinite = futures::stream::repeat_with(move || {
    pulled_for_stream.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Bytes::from_static(b"aaaa")
  })
  .then(|b| async move {
    tokio::time::sleep(Duration::from_millis(5)).await;
    b
  });

  let mut events = provider(&url)
    .transcribe_realtime(SttUplink::from_audio(infinite), cfg())
    .await
    .expect("stream opens");
  // 拿到 Started 说明泵已经在跑。
  let first = events.next().await;
  assert!(matches!(first, Some(Ok(SttEvent::Started { .. }))));
  tokio::time::sleep(Duration::from_millis(80)).await;
  drop(events);

  let after_drop = pulled.load(std::sync::atomic::Ordering::SeqCst);
  tokio::time::sleep(Duration::from_millis(200)).await;
  let later = pulled.load(std::sync::atomic::Ordering::SeqCst);
  assert_eq!(after_drop, later, "the uplink pump kept pulling audio after the consumer went away");
  server.abort();
}
