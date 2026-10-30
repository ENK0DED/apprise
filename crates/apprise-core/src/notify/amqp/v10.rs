//! AMQP 1.0 publishing with fe2o3-amqp.
//!
//! Upstream publishes with Apache Qpid Proton: one connection and session,
//! one link per target address, and a publish only succeeds once the broker
//! accepts the message.

use std::time::Duration;

use fe2o3_amqp::connection::ConnectionHandle;
use fe2o3_amqp::sasl_profile::SaslProfile;
use fe2o3_amqp::session::SessionHandle;
use fe2o3_amqp::types::messaging::{ApplicationProperties, Data, Header, Outcome, Priority, Properties};
use fe2o3_amqp::types::primitives::{Binary, SimpleValue, Symbol, Timestamp};
use fe2o3_amqp::{Connection, Sender, Session};
use tokio::time::timeout;

use super::{Config, Message, amqp_error, connect_stream};
use crate::error::NotifyError;

/// Proton never waits less than this, even when told to.
const MIN_TIMEOUT: Duration = Duration::from_millis(100);

pub(super) struct Publisher {
  config: Config,
  container_id: String,
  link: Option<(ConnectionHandle<()>, SessionHandle<()>)>,
}

impl Publisher {
  pub fn new(config: Config) -> Self {
    Self { config, container_id: random_uuid(), link: None }
  }

  fn connect_timeout(&self) -> Duration {
    self.config.connect_timeout.max(MIN_TIMEOUT)
  }

  fn read_timeout(&self) -> Duration {
    self.config.read_timeout.max(MIN_TIMEOUT)
  }

  pub async fn connect(&mut self) -> Result<(), NotifyError> {
    let cfg = &self.config;
    let stream = connect_stream(cfg).await?;

    // Log in with our user when we have one; otherwise anonymously
    let sasl = match &cfg.user {
      Some(user) => SaslProfile::Plain { username: user.clone(), password: cfg.password.clone().unwrap_or_default() },
      None => SaslProfile::Anonymous,
    };
    // The virtual host is the host name we ask for; TLS already checked the
    // certificate against the server we connected to
    let hostname = cfg.vhost.as_deref().unwrap_or(&cfg.host);
    let open = Connection::builder()
      .container_id(self.container_id.clone())
      .hostname(hostname)
      .sasl_hostname(hostname)
      .sasl_profile(sasl)
      .scheme("amqp")
      .open_with_stream(stream);
    let mut connection = timeout(self.connect_timeout(), open)
      .await
      .map_err(|_| amqp_error("timed out logging into the broker"))?
      .map_err(|e| amqp_error(format!("could not open the connection: {}", e)))?;

    // Every wait from here on is for a reply to something we sent
    let session = match timeout(self.read_timeout(), Session::begin(&mut connection)).await {
      Ok(Ok(session)) => session,
      result => {
        let _ = timeout(self.read_timeout(), connection.close()).await;
        return Err(match result {
          Ok(Err(e)) => amqp_error(format!("could not begin a session: {}", e)),
          _ => amqp_error("timed out beginning a session"),
        });
      }
    };
    self.link = Some((connection, session));
    Ok(())
  }

  pub async fn publish(&mut self, target: &str, message: &Message) -> Result<(), NotifyError> {
    let wait = self.read_timeout();
    let name = format!("{}-{}", self.container_id, target);
    let (_, session) = self.link.as_mut().ok_or_else(|| amqp_error("not connected"))?;

    // Open a link to the address we are publishing to
    let mut sender = timeout(wait, Sender::attach(session, name, target))
      .await
      .map_err(|_| amqp_error("timed out opening a link"))?
      .map_err(|e| amqp_error(format!("the address was refused: {}", e)))?;

    // Send our message and wait for the broker to accept it
    let outcome = timeout(wait, sender.send(build_message(message))).await;

    // Release our link; the connection stays open. Any error means the broker
    // already closed it for us.
    let _ = timeout(wait, sender.close()).await;

    match outcome {
      Ok(Ok(Outcome::Accepted(_))) => Ok(()),
      Ok(Ok(other)) => Err(amqp_error(format!("the broker did not accept the message: {:?}", other))),
      Ok(Err(e)) => Err(amqp_error(format!("the message was refused: {}", e))),
      Err(_) => Err(amqp_error("timed out waiting for the broker to accept the message")),
    }
  }

  pub async fn close(&mut self) {
    let wait = self.read_timeout();
    if let Some((mut connection, mut session)) = self.link.take() {
      // Give the broker our read timeout to say goodbye
      let _ = timeout(wait, session.end()).await;
      let _ = timeout(wait, connection.close()).await;
    }
  }
}

/// The AMQP 1.0 message: properties in their sections, the body as raw bytes
/// (a data section), so any client reads exactly the text we sent.
fn build_message(message: &Message) -> fe2o3_amqp::types::messaging::Message<Data> {
  // A header is only needed when something differs from the protocol defaults
  let header = (message.persistent || message.priority.is_some() || message.ttl.is_some()).then(|| Header {
    durable: message.persistent,
    priority: Priority(message.priority.unwrap_or(4)),
    // A ttl of 0 is kept: it means "expire now", not "no expiry"
    ttl: message.ttl,
    ..Default::default()
  });
  let properties = Properties {
    content_type: Some(Symbol::from(message.content_type)),
    creation_time: Some(Timestamp::from_milliseconds(message.timestamp as i64 * 1000)),
    ..Default::default()
  };
  let application_properties = (!message.headers.is_empty()).then(|| {
    let mut props = ApplicationProperties::default();
    for (key, value) in &message.headers {
      props.insert(key.clone(), SimpleValue::String(value.clone()));
    }
    props
  });

  fe2o3_amqp::types::messaging::Message::builder()
    .header(header)
    .properties(properties)
    .application_properties(application_properties)
    .data(Binary::from(message.body.clone()))
    .build()
}

/// A random (version 4) UUID, as proton uses for its container id.
fn random_uuid() -> String {
  let mut bytes: [u8; 16] = rand::random();
  bytes[6] = (bytes[6] & 0x0F) | 0x40;
  bytes[8] = (bytes[8] & 0x3F) | 0x80;
  let hex = hex::encode(bytes);
  format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::NotifyContext;
  use crate::notify::registry::from_url;
  use fe2o3_amqp::acceptor::sasl_acceptor::SaslServerFrame;
  use fe2o3_amqp::acceptor::{ConnectionAcceptor, LinkAcceptor, LinkEndpoint, SaslAcceptor, SessionAcceptor};
  use fe2o3_amqp::types::definitions::{AmqpError, Error as AmqpErrorBody};
  use fe2o3_amqp::types::messaging::Body;
  use fe2o3_amqp::types::primitives::{Array, Value};
  use fe2o3_amqp::types::sasl::{SaslCode, SaslInit, SaslOutcome, SaslResponse};
  use std::collections::BTreeMap;
  use std::sync::{Arc, Mutex};
  use tokio::net::TcpListener;

  /// What the broker saw: the SASL login and each (address, message).
  #[derive(Default)]
  struct Seen {
    mechanism: String,
    response: Vec<u8>,
    hostname: Option<String>,
    deliveries: Vec<(String, fe2o3_amqp::types::messaging::Message<Body<Value>>)>,
  }

  #[derive(Clone)]
  struct RecordingSasl(Arc<Mutex<Seen>>);

  impl SaslAcceptor for RecordingSasl {
    fn mechanisms(&self) -> Array<Symbol> {
      Array(vec![Symbol::from("PLAIN"), Symbol::from("ANONYMOUS")])
    }
    fn on_init(&mut self, init: SaslInit) -> SaslServerFrame {
      let mut seen = self.0.lock().unwrap();
      seen.mechanism = init.mechanism.0.clone();
      seen.response = init.initial_response.map(|r| r.into_vec()).unwrap_or_default();
      seen.hostname = init.hostname.clone();
      SaslServerFrame::Outcome(SaslOutcome { code: SaslCode::Ok, additional_data: None })
    }
    fn on_response(&mut self, _response: SaslResponse) -> SaslServerFrame {
      SaslServerFrame::Outcome(SaslOutcome { code: SaslCode::Auth, additional_data: None })
    }
  }

  /// A broker that accepts every message except those sent to `reject`,
  /// and refuses links to `refuse`.
  async fn broker(reject: &'static str, refuse: &'static str) -> (u16, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let shared = seen.clone();
    tokio::spawn(async move {
      while let Ok((stream, _)) = listener.accept().await {
        let acceptor = ConnectionAcceptor::builder().container_id("broker").sasl_acceptor(RecordingSasl(shared.clone())).build();
        let shared = shared.clone();
        tokio::spawn(async move {
          let Ok(mut connection) = acceptor.accept(stream).await else { return };
          let Ok(mut session) = SessionAcceptor::new().accept(&mut connection).await else { return };
          let links = LinkAcceptor::new();
          while let Ok(link) = links.accept(&mut session).await {
            let LinkEndpoint::Receiver(mut receiver) = link else { continue };
            let address = receiver.target().as_ref().and_then(|t| t.address.clone()).unwrap_or_default();
            if address == refuse {
              let error = AmqpErrorBody::new(AmqpError::NotFound, Some("no such queue".into()), None);
              let _ = receiver.close_with_error(error).await;
              continue;
            }
            let Ok(delivery) = receiver.recv::<Body<Value>>().await else { continue };
            if address == reject {
              let error = AmqpErrorBody::new(AmqpError::ResourceLimitExceeded, None, None);
              let _ = receiver.reject(&delivery, error).await;
            } else {
              let _ = receiver.accept(&delivery).await;
            }
            shared.lock().unwrap().deliveries.push((address, delivery.into_message()));
            let _ = receiver.close().await;
          }
          let _ = session.end().await;
          let _ = connection.close().await;
        });
      }
    });
    (port, seen)
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_build_message() {
    let message = Message {
      body: b"hello".to_vec(),
      content_type: "text/plain; charset=utf-8",
      app_id: "Apprise".into(),
      timestamp: 1234,
      persistent: true,
      priority: Some(7),
      ttl: Some(60000),
      headers: BTreeMap::from([("a".to_string(), "b".to_string())]),
    };
    let built = build_message(&message);
    let header = built.header.as_ref().unwrap();
    assert!(header.durable);
    assert_eq!(header.priority, Priority(7));
    assert_eq!(header.ttl, Some(60000));
    let props = built.properties.as_ref().unwrap();
    assert_eq!(props.content_type, Some(Symbol::from("text/plain; charset=utf-8")));
    assert_eq!(props.creation_time, Some(Timestamp::from_milliseconds(1_234_000)));
    assert_eq!(built.application_properties.as_ref().unwrap().get("a"), Some(&SimpleValue::String("b".into())));
    assert_eq!(built.body, Data(Binary::from(b"hello".to_vec())));

    // Without anything special the protocol defaults apply
    let plain = build_message(&Message { persistent: false, priority: None, ttl: None, headers: BTreeMap::new(), ..message.clone() });
    assert!(plain.header.is_none());
    assert!(plain.application_properties.is_none());

    // An explicit zero expiry is kept
    let zero = build_message(&Message { persistent: false, priority: None, ttl: Some(0), ..message });
    assert_eq!(zero.header.unwrap().ttl, Some(0));
  }

  #[test]
  fn test_random_uuid() {
    let id = random_uuid();
    assert_eq!(id.len(), 36);
    assert_eq!(&id[14..15], "4");
    assert_ne!(id, random_uuid());
  }

  #[tokio::test]
  async fn test_send() {
    let (port, seen) = broker("", "").await;
    let svc = from_url(&format!("amqp://user:pass@127.0.0.1:{}/plain?ver=1&to=/queues/a&priority=5&ttl=100&+source=nas&vhost=vhost:home", port)).unwrap();
    assert!(svc.send(&ctx("Title\r\nBody")).await.unwrap());

    let seen = seen.lock().unwrap();
    assert_eq!(seen.mechanism, "PLAIN");
    assert_eq!(seen.response, b"\0user\0pass");
    assert_eq!(seen.hostname.as_deref(), Some("vhost:home"));

    // One link per address, in upstream's (sorted) order
    let addresses: Vec<&str> = seen.deliveries.iter().map(|(a, _)| a.as_str()).collect();
    assert_eq!(addresses, vec!["/queues/a", "plain"]);

    let message = &seen.deliveries[0].1;
    let header = message.header.as_ref().unwrap();
    assert!(header.durable);
    assert_eq!(header.priority, Priority(5));
    assert_eq!(header.ttl, Some(100));
    let props = message.properties.as_ref().unwrap();
    assert_eq!(props.content_type, Some(Symbol::from("text/plain; charset=utf-8")));
    assert!(props.creation_time.is_some());
    assert_eq!(message.application_properties.as_ref().unwrap().get("source"), Some(&SimpleValue::String("nas".into())));
    match &message.body {
      Body::Data(batch) => assert_eq!(batch.iter().map(|d| d.0.to_vec()).collect::<Vec<_>>(), vec![b"Title\r\nBody".to_vec()]),
      other => panic!("unexpected body {:?}", other),
    }
  }

  #[tokio::test]
  async fn test_send_anonymous() {
    let (port, seen) = broker("", "").await;
    let svc = from_url(&format!("amqp://127.0.0.1:{}/?version=1.0&to=/queues/alerts&persistent=no", port)).unwrap();
    assert!(svc.send(&ctx("Body")).await.unwrap());

    let seen = seen.lock().unwrap();
    assert_eq!(seen.mechanism, "ANONYMOUS");
    assert_eq!(seen.hostname.as_deref(), Some("127.0.0.1"));
    assert!(seen.deliveries[0].1.header.is_none());
  }

  #[tokio::test]
  async fn test_refusals() {
    let (port, seen) = broker("rejected", "missing").await;
    let svc = from_url(&format!("amqp://127.0.0.1:{}/good/missing/rejected?ver=1", port)).unwrap();
    // The other targets are still tried
    assert!(!svc.send(&ctx("Body")).await.unwrap());
    let addresses: Vec<String> = seen.lock().unwrap().deliveries.iter().map(|(a, _)| a.clone()).collect();
    assert_eq!(addresses, vec!["good", "rejected"]);
  }

  #[tokio::test]
  async fn test_connect_failures() {
    // A broker that never answers
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
      let mut held = Vec::new();
      while let Ok((stream, _)) = listener.accept().await {
        held.push(stream);
      }
    });
    let svc = from_url(&format!("amqp://127.0.0.1:{}/q?ver=1&cto=0.2&rto=0.2", port)).unwrap();
    let started = std::time::Instant::now();
    assert!(svc.send(&ctx("Body")).await.is_err());
    assert!(started.elapsed() < Duration::from_secs(2));

    // Nothing listening (a freed test port could be taken by a parallel test)
    assert!(from_url("amqp://127.0.0.1:1/q?ver=1").unwrap().send(&ctx("Body")).await.is_err());
  }

  #[tokio::test]
  async fn test_tls() {
    let acceptor = super::super::tests::test_acceptor();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let shared = seen.clone();
    tokio::spawn(async move {
      while let Ok((stream, _)) = listener.accept().await {
        let Ok(tls) = acceptor.accept(stream).await else { continue };
        let amqp = ConnectionAcceptor::builder().container_id("broker").sasl_acceptor(RecordingSasl(shared.clone())).build();
        let shared = shared.clone();
        tokio::spawn(async move {
          let Ok(mut connection) = amqp.accept(tls).await else { return };
          let Ok(mut session) = SessionAcceptor::new().accept(&mut connection).await else { return };
          if let Ok(LinkEndpoint::Receiver(mut receiver)) = LinkAcceptor::new().accept(&mut session).await
            && let Ok(delivery) = receiver.recv::<Body<Value>>().await
          {
            let _ = receiver.accept(&delivery).await;
            shared.lock().unwrap().deliveries.push((String::new(), delivery.into_message()));
            let _ = receiver.close().await;
          }
          let _ = session.end().await;
          let _ = connection.close().await;
        });
      }
    });

    // The self-signed certificate is refused unless verify=no
    let svc = from_url(&format!("amqps://localhost:{}/q?ver=1", port)).unwrap();
    assert!(svc.send(&ctx("Body")).await.is_err());
    let svc = from_url(&format!("amqps://localhost:{}/q?ver=1&verify=no", port)).unwrap();
    assert!(svc.send(&ctx("Body")).await.unwrap());
    assert_eq!(seen.lock().unwrap().deliveries.len(), 1);
  }
}
