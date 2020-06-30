use crate::{
    p2p::{self, P2PConnection},
    Config,
    RelayStream,
    SshClient,
    SshCredentials,
    SshServer,
    TunnelStream, //, UdpConnection,
};
use anyhow::{Error, Result};
use futures::stream::StreamExt;
use log::*;
use std::net::ToSocketAddrs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::TcpStream;
use tokio_rustls::{client::TlsStream, rustls::ClientConfig, TlsConnector};
use tokio_util::compat::*;
use tunshell_shared::*;
use webpki::DNSNameRef;

pub type ClientMessageStream =
    MessageStream<ClientMessage, ServerMessage, Compat<TlsStream<TcpStream>>>;

pub struct Client<'a> {
    config: &'a Config,
}

impl<'a> Client<'a> {
    pub fn new(config: &'a Config) -> Self {
        Self { config }
    }

    pub async fn start_session(&mut self) -> Result<u8> {
        println!("Connecting to relay server...");
        let relay_socket: TlsStream<TcpStream> = self.connect_to_relay().await?;

        let mut message_stream = ClientMessageStream::new(relay_socket.compat());

        let key_type = self.send_key(&mut message_stream).await?;

        println!("Waiting for peer to join...");
        let mut peer_info = self.wait_for_peer_to_join(&mut message_stream).await?;
        println!("{} joined the session", peer_info.peer_ip_address);

        println!("Negotiating connection...");
        let message_stream = Arc::new(Mutex::new(message_stream));
        let peer_socket = self
            .negotiate_peer_connect(&message_stream, &mut peer_info, key_type.is_host())
            .await?;

        let exit_code = match key_type {
            KeyType::Host => SshServer::new()?
                .run(
                    peer_socket,
                    SshCredentials::new("tunshell", self.config.client_key()),
                )
                .await
                .and_then(|_| Ok(0))?,
            KeyType::Client => {
                SshClient::new()?
                    .connect(
                        peer_socket,
                        SshCredentials::new("tunshell", &peer_info.peer_key),
                    )
                    .await?
            }
        };

        Ok(exit_code)
    }

    async fn connect_to_relay(&mut self) -> Result<TlsStream<TcpStream>> {
        let mut config = ClientConfig::default();
        config
            .root_store
            .add_server_trust_anchors(&webpki_roots::TLS_SERVER_ROOTS);
        let connector = TlsConnector::from(Arc::new(config));

        let relay_dns_name = DNSNameRef::try_from_ascii_str(self.config.relay_host())?;
        let relay_addr = (self.config.relay_host(), self.config.relay_port())
            .to_socket_addrs()?
            .next()
            .unwrap();

        let network_stream = TcpStream::connect(&relay_addr).await?;
        network_stream.set_keepalive(Some(Duration::from_secs(30)))?;
        let transport_stream = connector.connect(relay_dns_name, network_stream).await?;

        Ok(transport_stream)
    }

    async fn send_key(&self, message_stream: &mut ClientMessageStream) -> Result<KeyType> {
        message_stream
            .write(&ClientMessage::Key(KeyPayload {
                key: self.config.client_key().to_owned(),
            }))
            .await?;

        match message_stream.next().await {
            Some(Ok(ServerMessage::KeyAccepted(payload))) => Ok(payload.key_type),
            Some(Ok(ServerMessage::KeyRejected)) => {
                Err(Error::msg("The session key has expired or is invalid"))
            }
            Some(Ok(message)) => Err(Error::msg(format!(
                "Unexpected response returned by server: {:?}",
                message
            ))),
            Some(Err(err)) => return Err(err),
            None => return Err(Error::msg("Connection closed unexpectedly")),
        }
    }

    async fn wait_for_peer_to_join(
        &mut self,
        message_stream: &mut ClientMessageStream,
    ) -> Result<PeerJoinedPayload> {
        match message_stream.next().await {
            Some(Ok(ServerMessage::PeerJoined(payload))) => Ok(payload),
            _ => Err(Error::msg("Unexpected response returned by server")),
        }
    }

    async fn negotiate_peer_connect(
        &mut self,
        message_stream: &Arc<Mutex<ClientMessageStream>>,
        peer_info: &mut PeerJoinedPayload,
        master_side: bool,
    ) -> Result<Box<dyn TunnelStream>> {
        loop {
            let mut message_stream = message_stream.lock().unwrap();
            match message_stream.next().await {
                Some(Ok(ServerMessage::TimePlease)) => {
                    message_stream
                        .write(&ClientMessage::Time(TimePayload {
                            client_time: SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_millis() as u64,
                        }))
                        .await?
                }
                Some(Ok(ServerMessage::AttemptDirectConnect(payload))) => {
                    match self
                        .attempt_direct_connection(
                            &mut message_stream,
                            peer_info,
                            &payload,
                            master_side,
                        )
                        .await?
                    {
                        Some(direct_stream) => {
                            println!("Direct connection to peer established");
                            return Ok(direct_stream);
                        }
                        None => {
                            message_stream
                                .write(&ClientMessage::DirectConnectFailed)
                                .await?
                        }
                    }
                }
                Some(Ok(ServerMessage::StartRelayMode)) => break,
                Some(Ok(message)) => {
                    return Err(Error::msg(format!(
                        "Unexpected response returned by server: {:?}",
                        message
                    )))
                }
                Some(Err(err)) => return Err(err),
                None => return Err(Error::msg("Connection closed unexpectedly")),
            }
        }

        println!("Falling back to relayed connection");
        Ok(Box::new(RelayStream::new(Arc::clone(message_stream))))
    }

    async fn attempt_direct_connection(
        &mut self,
        _message_stream: &mut ClientMessageStream,
        peer_info: &mut PeerJoinedPayload,
        connection_info: &AttemptDirectConnectPayload,
        master_side: bool,
    ) -> Result<Option<Box<dyn TunnelStream>>> {
        println!(
            "Attempting direct connection to {}",
            peer_info.peer_ip_address
        );

        // Initialise and bind connections
        let mut tcp = p2p::tcp::TcpConnection::new(peer_info.clone(), connection_info.clone());
        let mut udp =
            p2p::udp_adaptor::UdpConnectionAdaptor::new(peer_info.clone(), connection_info.clone());

        tokio::try_join!(tcp.bind() /*, udp.bind() */)?;

        let current_timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let sleep_duration = std::cmp::max(0, connection_info.connect_at - current_timestamp);

        std::thread::sleep(Duration::from_millis(sleep_duration));

        tokio::select! {
            result = tcp.connect(master_side) => match result {
                Ok(_) => return Ok(Some(Box::new(tcp))),
                Err(err) => error!("Error while establishing TCP connection: {}", err)
            },
            result = udp.connect(master_side) => match result {
                Ok(_) => return Ok(Some(Box::new(udp))),
                Err(err) => error!("Error while establishing UDP connection: {}", err)
            }
        };

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::runtime::Runtime;

    #[test]
    fn test_connect_to_relay_server() {
        let config = Config::new("test", "relay.tunshell.com", 5000);
        let mut client = Client::new(&config);

        let result = Runtime::new().unwrap().block_on(client.connect_to_relay());

        result.unwrap();
    }

    #[test]
    fn test_send_invalid_key() {
        let config = Config::new("invalid_key", "relay.tunshell.com", 5000);
        let mut client = Client::new(&config);

        let result = Runtime::new().unwrap().block_on(client.start_session());

        match result {
            Ok(_) => panic!("should not return ok"),
            Err(err) => assert_eq!(err.to_string(), "The session key has expired or is invalid"),
        }
    }
}
