use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::channel::mpsc;
use futures::never::Never;
use futures::{FutureExt, SinkExt, StreamExt, future, stream};
use irc::proto::{self, Command, command};
use irc::{Connection, codec, connection};
use tokio::time::{self, Instant, Interval};
#[cfg(feature = "veilid")]
use veilid::{Codec as VeilidCodec, Connection as VeilidConnection};

use crate::client::Client;
use crate::server::Server;
use crate::time::Posix;
use crate::{config, message, server};

const QUIT_REQUEST_TIMEOUT: Duration = Duration::from_millis(400);

pub type Result<T = Update, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Connection(#[from] connection::Error),
    #[cfg(feature = "veilid")]
    #[error(transparent)]
    VeilidConnection(#[from] veilid::connection::Error),
}

#[derive(Debug)]
pub enum Update {
    Connected {
        server: Server,
        client: Client,
        is_initial: bool,
        sent_time: DateTime<Utc>,
    },
    Disconnected {
        server: Server,
        is_initial: bool,
        error: Option<String>,
        sent_time: DateTime<Utc>,
    },
    ConnectionFailed {
        server: Server,
        error: String,
        sent_time: DateTime<Utc>,
    },
    MessagesReceived(Server, Vec<message::Encoded>),
    Quit(Server, Option<String>),
}

enum State {
    Disconnected {
        last_retry: Option<Instant>,
    },
    Connected {
        stream: ProtocolStream,
        batch: Batch,
        ping_time: Interval,
        ping_timeout: Option<Interval>,
        quit_requested: Option<(Instant, Option<String>)>,
    },
    Quit,
}

enum Input {
    IrcMessage(Result<codec::ParseResult, codec::Error>),
    #[cfg(feature = "veilid")]
    VeilidMessage(Result<String, veilid::codec::Error>),
    Batch(Vec<message::Encoded>),
    Send(proto::Message),
    Ping,
    PingTimeout,
    Quit(Option<String>),
}

enum ProtocolStream {
    Irc(StreamIrc),
    #[cfg(feature = "veilid")]
    Veilid(StreamVeilid),
}

struct StreamIrc {
    connection: Connection<irc::Codec>,
    receiver: mpsc::Receiver<proto::Message>,
}

#[cfg(feature = "veilid")]
struct StreamVeilid {
    connection: VeilidConnection,
    receiver: mpsc::Receiver<proto::Message>,
}

pub fn run(
    server: server::Entry,
    proxy: Option<config::Proxy>,
) -> impl futures::Stream<Item = Update> {
    let (sender, receiver) = mpsc::unbounded();

    // Spawn to unblock backend from iced stream which has backpressure
    let runner =
        stream::once(async { tokio::spawn(_run(server, proxy, sender)).await })
            .map(|_| unreachable!());

    stream::select(receiver, runner)
}

async fn _run(
    server: server::Entry,
    proxy: Option<config::Proxy>,
    sender: mpsc::UnboundedSender<Update>,
) -> Never {
    let server::Entry { server, config } = server;

    let reconnect_delay = Duration::from_secs(config.reconnect_delay);

    let mut is_initial = true;
    let mut state = State::Disconnected { last_retry: None };

    // Notify app of initial disconnected state
    let _ = sender.unbounded_send(Update::Disconnected {
        server: server.clone(),
        is_initial,
        error: None,
        sent_time: Utc::now(),
    });

    loop {
        match &mut state {
            State::Disconnected { last_retry } => {
                if let Some(last_retry) = last_retry.as_ref() {
                    let remaining =
                        reconnect_delay.saturating_sub(last_retry.elapsed());

                    if !remaining.is_zero() {
                        time::sleep(remaining).await;
                    }
                }

                match connect(server.clone(), config.clone(), proxy.clone())
                    .await
                {
                    Ok((stream, client)) => {
                        log::info!("[{server}] connected");

                        let _ = sender.unbounded_send(Update::Connected {
                            server: server.clone(),
                            client,
                            is_initial,
                            sent_time: Utc::now(),
                        });

                        is_initial = false;

                        state = State::Connected {
                            stream,
                            batch: Batch::new(),
                            ping_timeout: None,
                            ping_time: ping_time_interval(config.ping_time),
                            quit_requested: None,
                        };
                    }
                    Err(e) => {
                        let error = match &e {
                            // unwrap Tls-specific error enums to access more error info
                            Error::Connection(connection::Error::Tls(e)) => {
                                format!("a TLS error occurred: {e}")
                            }
                            _ => format!("{e:?}"),
                        };

                        log::info!("[{server}] connection failed: {error}");

                        let _ =
                            sender.unbounded_send(Update::ConnectionFailed {
                                server: server.clone(),
                                error,
                                sent_time: Utc::now(),
                            });

                        *last_retry = Some(Instant::now());
                    }
                }
            }
            State::Connected {
                stream,
                batch,
                ping_time,
                ping_timeout,
                quit_requested,
            } => {
                let input = match stream {
                    ProtocolStream::Irc(stream_irc) => {
                        let mut select = stream::select_all([
                            (&mut stream_irc.connection)
                                .map(Input::IrcMessage)
                                .boxed(),
                            (&mut stream_irc.receiver).map(Input::Send).boxed(),
                            ping_time
                                .tick()
                                .into_stream()
                                .map(|_| Input::Ping)
                                .boxed(),
                            batch.map(Input::Batch).boxed(),
                        ]);
                        if let Some(timeout) = ping_timeout.as_mut() {
                            select.push(
                                timeout
                                    .tick()
                                    .into_stream()
                                    .map(|_| Input::PingTimeout)
                                    .boxed(),
                            );
                        }
                        if let Some((requested_at, reason)) = quit_requested {
                            select.push(
                                time::sleep_until(
                                    *requested_at + QUIT_REQUEST_TIMEOUT,
                                )
                                .into_stream()
                                .map(|()| Input::Quit(reason.clone()))
                                .boxed(),
                            );
                        }
                        select.next().await.expect("stream input")
                    }
                    #[cfg(feature = "veilid")]
                    ProtocolStream::Veilid(stream_veilid) => {
                        let mut select = stream::select_all([
                            (&mut stream_veilid.connection)
                                .map(|msg| {
                                    // Convert veilid string message to IRC-like message
                                    // This is a placeholder - actual implementation would parse veilid protocol
                                    let outme = msg.unwrap(); // yolo
                                    Input::VeilidMessage(Ok(outme))
                                })
                                .boxed(),
                            (&mut stream_veilid.receiver)
                                .map(Input::Send)
                                .boxed(),
                            batch.map(Input::Batch).boxed(),
                        ]);
                        select.next().await.expect("stream input")
                    }
                };

                match input {
                    Input::IrcMessage(Ok(Ok(message))) => {
                        #[allow(unreachable_patterns)]
                        match stream {
                            ProtocolStream::Irc(stream_irc) => {
                                match message.command {
                                    proto::Command::PING(token) => {
                                        let _ = stream_irc
                                            .connection
                                            .send(command!("PONG", token))
                                            .await;
                                    }
                                    proto::Command::PONG(_, token) => {
                                        let token = token.unwrap_or_default();
                                        log::trace!(
                                            "[{server}] pong received: {token}"
                                        );

                                        *ping_timeout = None;
                                    }
                                    proto::Command::ERROR(error) => {
                                        if let Some(reason) = quit_requested
                                            .as_ref()
                                            .map(|(_, reason)| reason)
                                        {
                                            // If QUIT was requested, then ERROR is
                                            // a valid acknowledgement
                                            // https://modern.ircdocs.horse/#quit-message
                                            let _ = sender.unbounded_send(
                                                Update::Quit(
                                                    server.clone(),
                                                    reason.clone(),
                                                ),
                                            );

                                            state = State::Quit;
                                        } else {
                                            log::info!(
                                                "[{server}] disconnected: {error}"
                                            );
                                            let _ = sender.unbounded_send(
                                                Update::Disconnected {
                                                    server: server.clone(),
                                                    is_initial,
                                                    error: Some(error),
                                                    sent_time: Utc::now(),
                                                },
                                            );
                                            state = State::Disconnected {
                                                last_retry: Some(Instant::now()),
                                            };
                                        }
                                    }
                                    _ => {
                                        batch.messages.push(message.into());
                                    }
                                }
                            }
                            #[cfg(feature = "veilid")]
                            ProtocolStream::Veilid(_) => {
                                // IRC messages shouldn't be received on Veilid stream
                                log::warn!(
                                    "Received IRC message on Veilid stream"
                                );
                            }
                            #[cfg(not(feature = "veilid"))]
                            _ => unreachable!(),
                        }
                    }
                    Input::IrcMessage(Ok(Err(e))) => {
                        log::warn!("message decoding failed: {e}");
                    }
                    Input::IrcMessage(Err(e)) => {
                        log::info!("[{server}] disconnected: {e}");
                        let _ = sender.unbounded_send(Update::Disconnected {
                            server: server.clone(),
                            is_initial,
                            error: Some(e.to_string()),
                            sent_time: Utc::now(),
                        });
                        state = State::Disconnected {
                            last_retry: Some(Instant::now()),
                        };
                    }
                    #[cfg(feature = "veilid")]
                    Input::VeilidMessage(Ok(msg)) => {
                        // Convert veilid message to IRC-like message for compatibility
                        // This is a placeholder - actual implementation would parse veilid protocol
                        // and convert to message::Encoded format
                        #[allow(unreachable_patterns)]
                        match stream {
                            ProtocolStream::Veilid(_) => {
                                // TODO: Parse veilid message format and convert to message::Encoded
                                // For now, we'll skip processing until actual veilid protocol is implemented
                                log::trace!(
                                    "[{server}] Veilid message received: {msg}"
                                );
                                // Example conversion would be:
                                // let encoded = parse_veilid_message(&msg)?;
                                // batch.messages.push(encoded);
                            }
                            ProtocolStream::Irc(_) => {
                                log::warn!(
                                    "Received Veilid message on IRC stream"
                                );
                            }
                        }
                    }
                    #[cfg(feature = "veilid")]
                    Input::VeilidMessage(Err(e)) => {
                        log::info!("[{server}] veilid message error: {e}");
                        let _ = sender.unbounded_send(Update::Disconnected {
                            server: server.clone(),
                            is_initial,
                            error: Some(e.to_string()),
                            sent_time: Utc::now(),
                        });
                        state = State::Disconnected {
                            last_retry: Some(Instant::now()),
                        };
                    }
                    Input::Batch(messages) => {
                        let _ = sender.unbounded_send(
                            Update::MessagesReceived(server.clone(), messages),
                        );
                    }
                    Input::Send(message) => {
                        #[allow(unreachable_patterns)]
                        match stream {
                            ProtocolStream::Irc(stream_irc) => {
                                log::trace!(
                                    "[{server}] Sending IRC message => {message:?}"
                                );

                                if let Command::QUIT(reason) = &message.command
                                {
                                    let reason = reason.clone();

                                    let _ = stream_irc
                                        .connection
                                        .send(message)
                                        .await;

                                    log::info!("[{server}] quit");

                                    *quit_requested =
                                        Some((Instant::now(), reason));
                                } else {
                                    let _ = stream_irc
                                        .connection
                                        .send(message)
                                        .await;
                                }
                            }
                            #[cfg(feature = "veilid")]
                            ProtocolStream::Veilid(stream_veilid) => {
                                // Convert IRC message to veilid protocol format
                                // Extract the message content from IRC PRIVMSG command
                                if let Command::PRIVMSG(target, msg) =
                                    &message.command
                                {
                                    let message_text =
                                        format!("{}: {}", target, msg);
                                    if let Err(e) = stream_veilid
                                        .connection
                                        .send_message(message_text.clone())
                                        .await
                                    {
                                        log::error!(
                                            "Failed to send veilid message: {e:?}"
                                        );
                                    } else {
                                        log::trace!(
                                            "[{server}] Sent veilid message: {message_text}"
                                        );
                                    }
                                } else {
                                    log::trace!(
                                        "[{server}] Ignoring non-PRIVMSG command for veilid: {message:?}"
                                    );
                                }
                            }
                            #[cfg(not(feature = "veilid"))]
                            _ => unreachable!(),
                        }
                    }
                    Input::Ping => {
                        #[allow(unreachable_patterns)]
                        match stream {
                            ProtocolStream::Irc(stream_irc) => {
                                let now = Posix::now().as_nanos().to_string();
                                log::trace!("[{server}] ping sent: {now}");

                                let _ = stream_irc
                                    .connection
                                    .send(command!("PING", now))
                                    .await;

                                if ping_timeout.is_none() {
                                    *ping_timeout =
                                        Some(ping_timeout_interval(
                                            config.ping_timeout,
                                        ));
                                }
                            }
                            #[cfg(feature = "veilid")]
                            ProtocolStream::Veilid(_) => {
                                // Veilid doesn't use ping/pong
                            }
                            #[cfg(not(feature = "veilid"))]
                            _ => unreachable!(),
                        }
                    }
                    Input::PingTimeout => {
                        log::info!("[{server}] ping timeout");
                        let _ = sender.unbounded_send(Update::Disconnected {
                            server: server.clone(),
                            is_initial,
                            error: Some("ping timeout".into()),
                            sent_time: Utc::now(),
                        });
                        state = State::Disconnected {
                            last_retry: Some(Instant::now()),
                        };
                    }
                    Input::Quit(reason) => {
                        let _ = sender.unbounded_send(Update::Quit(
                            server.clone(),
                            reason,
                        ));

                        state = State::Quit;
                    }
                }
            }
            State::Quit => {
                // Wait forever until this stream is dropped by the frontend
                future::pending::<()>().await;
            }
        }
    }
}

async fn connect(
    server: Server,
    config: Arc<config::Server>,
    proxy: Option<config::Proxy>,
) -> Result<(ProtocolStream, Client), Error> {
    match config.protocol {
        config::server::Protocol::Irc => {
            let connection_config = config.connection(proxy).map_err(|e| {
                Error::Connection(connection::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    e.to_string(),
                )))
            })?;
            let connection = Connection::new(connection_config, irc::Codec)
                .await
                .map_err(Error::Connection)?;

            let (sender, receiver) = mpsc::channel(100);

            let mut client = Client::new(server, config, sender);
            if let Err(e) = client.connect() {
                log::error!("Error when connecting client: {e:?}");
            }

            Ok((
                ProtocolStream::Irc(StreamIrc {
                    connection,
                    receiver,
                }),
                client,
            ))
        }
        #[cfg(feature = "veilid")]
        config::server::Protocol::Veilid => {
            use crate::environment;
            use std::path::PathBuf;

            let route_blob = config.veilid_route_blob().map_err(|e| {
                Error::VeilidConnection(veilid::connection::Error::Io(
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        e.to_string(),
                    ),
                ))
            })?;

            let data_dir = environment::data_dir();

            let connection = VeilidConnection::new(
                veilid::connection::Config {
                    route_blob,
                    data_dir,
                },
                VeilidCodec,
            )
            .await
            .map_err(Error::VeilidConnection)?;

            // For veilid, we still use IRC message format for compatibility
            // but we'll need to convert veilid protocol messages to IRC format
            let (sender, receiver) = mpsc::channel(100);

            // For veilid, we create a minimal client
            // Since veilid is user-to-user, we don't need full IRC client functionality
            let mut client = Client::new(server, config, sender);
            if let Err(e) = client.connect() {
                log::error!("Error when connecting client: {e:?}");
            }

            Ok((
                ProtocolStream::Veilid(StreamVeilid {
                    connection,
                    receiver,
                }),
                client,
            ))
        }
        #[cfg(not(feature = "veilid"))]
        config::server::Protocol::Veilid => {
            return Err(Error::Connection(connection::Error::Io(
                std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "Veilid protocol support is not enabled. Build with --features veilid to enable it.",
                ),
            )));
        }
    }
}

struct Batch {
    interval: Interval,
    messages: Vec<message::Encoded>,
}

impl Batch {
    const INTERVAL_MILLIS: u64 = 50;

    fn new() -> Self {
        Self {
            interval: time::interval_at(
                Instant::now() + Duration::from_millis(Self::INTERVAL_MILLIS),
                Duration::from_millis(Self::INTERVAL_MILLIS),
            ),
            messages: vec![],
        }
    }
}

impl futures::Stream for Batch {
    type Item = Vec<message::Encoded>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let batch = self.get_mut();

        match batch.interval.poll_tick(cx) {
            std::task::Poll::Ready(_) => {
                let messages = std::mem::take(&mut batch.messages);

                if messages.is_empty() {
                    std::task::Poll::Pending
                } else {
                    std::task::Poll::Ready(Some(messages))
                }
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

fn ping_time_interval(secs: u64) -> Interval {
    time::interval_at(
        Instant::now() + Duration::from_secs(secs),
        Duration::from_secs(secs),
    )
}

fn ping_timeout_interval(secs: u64) -> Interval {
    time::interval_at(
        Instant::now() + Duration::from_secs(secs),
        Duration::from_secs(secs),
    )
}
