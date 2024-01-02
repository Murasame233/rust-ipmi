use std::{
    collections::HashMap,
    net::{ToSocketAddrs, UdpSocket},
};

use crate::{
    err::IPMIClientError,
    helpers::utils::{append_u128_to_vec, append_u32_to_vec, hash_hmac_sha_256},
    ipmi::{
        data::{
            app::{
                channel::{
                    AuthVersion, GetChannelAuthCapabilitiesRequest,
                    GetChannelAuthCapabilitiesResponse, Privilege, KG,
                },
                cipher::{GetChannelCipherSuitesRequest, GetChannelCipherSuitesResponse},
            },
            commands::Command,
        },
        ipmi_header::AuthType,
        ipmi_v2_header::PayloadType,
        payload::{
            self,
            ipmi_payload::IpmiPayload,
            ipmi_payload_response::{CompletionCode, IpmiPayloadResponse},
        },
        rmcp_payloads::{
            rakp::{RAKPMessage1, RAKPMessage2, RAKPMessage3, RAKP},
            rmcp_open_session::{
                AuthAlgorithm, ConfidentialityAlgorithm, IntegrityAlgorithm, RMCPPlusOpenSession,
                RMCPPlusOpenSessionRequest, StatusCode,
            },
        },
    },
    packet::packet::{Packet, Payload},
};

type Result<T> = core::result::Result<T, IPMIClientError>;

#[derive(Debug)]
pub struct IPMIClient {
    client_socket: UdpSocket,
    auth_state: AuthState,
    command_state: Option<CommandState>,
    channel_number: Option<u8>,
    auth_algorithm: Option<AuthAlgorithm>,
    integrity_algorithm: Option<IntegrityAlgorithm>,
    confidentiality_algorithm: Option<ConfidentialityAlgorithm>,
    managed_system_session_id: Option<u32>,
    managed_system_random_number: Option<u128>,
    managed_system_guid: Option<u128>,
    remote_console_session_id: Option<u32>,
    remote_console_random_number: u128,
    username: Option<String>,
    password_mac_key: Option<Vec<u8>>,
    channel_auth_capabilities: Option<GetChannelAuthCapabilitiesResponse>,
    cipher_suite_bytes: Option<Vec<u8>>,
    cipher_list_index: u8,
    sik: Option<[u8; 32]>,
    k1: Option<[u8; 32]>,
    k2: Option<[u8; 32]>,
}
impl IPMIClient {
    /// Creates client for running IPMI commands against a BMC.
    ///
    /// # Arguments
    /// * `ipmi_server_addr` - Socket address of the IPMI server (or BMC LAN controller). Default port for IPMI RMCP is 623 UDP.
    ///
    /// # Examples
    ///
    /// ```
    /// use rust_ipmi::ipmi_client::IPMIClient;
    ///
    /// let ipmi_server = "192.168.1.10:623"
    /// let ipmi_client: Result<IPMIClient, IPMIClientError> = IPMIClient::new(ipmi_server)
    /// ```
    pub fn new<A: ToSocketAddrs>(ipmi_server_addr: A) -> Result<IPMIClient> {
        let client_socket =
            UdpSocket::bind("0.0.0.0:0").map_err(|e| IPMIClientError::FailedBind(e))?;
        client_socket
            .connect(ipmi_server_addr)
            .map_err(|e| IPMIClientError::ConnectToIPMIServer(e))?;
        Ok(IPMIClient {
            client_socket,
            auth_state: AuthState::Discovery,
            command_state: None,
            auth_algorithm: None,
            integrity_algorithm: None,
            confidentiality_algorithm: None,
            managed_system_session_id: None,
            managed_system_guid: None,
            remote_console_random_number: rand::random::<u128>(),
            username: None,
            password_mac_key: None,
            sik: None,
            k1: None,
            k2: None,
            channel_number: None,
            channel_auth_capabilities: None,
            cipher_suite_bytes: None,
            cipher_list_index: 0,
            managed_system_random_number: None,
            remote_console_session_id: None,
        })
    }

    /// Authenticates and establishes a session with the BMC.
    ///
    /// # Arguments
    /// * `username` - username used to authenticate against the BMC.
    /// * `password` - password for the username provided.
    ///
    /// # Examples
    ///
    /// ```
    /// use rust_ipmi::ipmi_client::IPMIClient;
    ///
    /// let ipmi_server = "192.168.1.10:623";
    /// let mut ipmi_client: IPMIClient = IPMIClient::new(ipmi_server)
    ///     .expect("Failed to connect to the server");
    ///
    /// let username = "my-username";
    /// ipmi_client.establish_connection(username, "password")
    ///     .expect("Failed to establish session with BMC");
    ///
    ///
    /// ```
    pub fn establish_connection<S: ToString>(&mut self, username: S, password: S) -> Result<()> {
        // if let Some(u) = username {
        // };
        self.username = Some(username.to_string());
        // if let Some(u) = password {
        // };
        let binding = password.to_string();
        let rakp2_mac_key = binding.as_bytes();
        self.password_mac_key = Some(rakp2_mac_key.into());

        self.discovery_request()?; // Get channel auth capabilites and set cipher
        self.authenticate()?; // rmcp open session and authenticate

        Ok(())
    }

    fn discovery_request(&mut self) -> Result<()> {
        let channel_packet = GetChannelAuthCapabilitiesRequest::new(true, Privilege::Administrator)
            .create_packet(AuthType::None, 0x0, 0x0, None);
        self.send_packet(channel_packet)?;

        // Get the Channel Cipher Suites
        let cipher_packet = GetChannelCipherSuitesRequest::default().create_packet();
        self.send_packet(cipher_packet)?;
        Ok(())
    }

    fn authenticate(&mut self) -> Result<()> {
        // RMCP+ Open Session Request
        let rmcp_open_packet = RMCPPlusOpenSessionRequest::new(
            0,
            Privilege::Administrator,
            0xa0a2a3a4,
            self.auth_algorithm.clone().unwrap(),
            self.integrity_algorithm.clone().unwrap(),
            self.confidentiality_algorithm.clone().unwrap(),
        )
        .create_packet();
        self.send_packet(rmcp_open_packet)?;

        // RAKP Message 1
        let rakp1_packet = RAKPMessage1::new(
            0x0,
            self.managed_system_session_id.unwrap(),
            self.remote_console_random_number,
            true,
            Privilege::Administrator,
            self.username.clone().unwrap(),
        )
        .create_packet();
        self.send_packet(rakp1_packet)?;
        self.create_session_keys()?;

        // RAKP Message 3
        let mut rakp3_input_buffer = Vec::new();
        append_u128_to_vec(
            &mut rakp3_input_buffer,
            self.managed_system_random_number.unwrap(),
        );
        append_u32_to_vec(
            &mut rakp3_input_buffer,
            self.remote_console_session_id.unwrap(),
        );
        rakp3_input_buffer.push(0x14);
        rakp3_input_buffer.push(self.username.clone().unwrap().len().try_into().unwrap());
        self.username
            .clone()
            .unwrap()
            .as_bytes()
            .iter()
            .for_each(|char| rakp3_input_buffer.push(char.clone()));

        let rakp3_auth_code =
            hash_hmac_sha_256(self.password_mac_key.clone().unwrap(), rakp3_input_buffer);
        let rakp3_packet = RAKPMessage3::new(
            0x0,
            StatusCode::NoErrors,
            self.managed_system_session_id.clone().unwrap(),
            Some(rakp3_auth_code.into()),
        )
        .create_packet();
        self.send_packet(rakp3_packet)?;

        Ok(())
    }

    fn send_packet(&mut self, request_packet: Packet) -> Result<()> {
        self.client_socket
            .send(&request_packet.to_bytes())
            .map_err(|e| IPMIClientError::FailedSend(e))?;

        let mut recv_buff = [0; 8092];

        if let Ok((n_bytes, _addr)) = self.client_socket.recv_from(&mut recv_buff) {
            let response_slice = &recv_buff[..n_bytes];
            let response_packet = Packet::try_from(response_slice)?;

            if let Some(payload) = response_packet.payload {
                match payload {
                    Payload::Ipmi(IpmiPayload::Request(_)) => {
                        return Err(IPMIClientError::MisformedResponse)
                    }
                    Payload::Ipmi(IpmiPayload::Response(payload)) => {
                        self.handle_completion_code(payload)?
                    }
                    Payload::RMCP(RMCPPlusOpenSession::Request(_)) => {
                        return Err(IPMIClientError::MisformedResponse)
                    }
                    Payload::RMCP(RMCPPlusOpenSession::Response(payload)) => self
                        .handle_status_code(Payload::RMCP(RMCPPlusOpenSession::Response(
                            payload,
                        )))?,
                    Payload::RAKP(RAKP::Message2(payload)) => {
                        self.handle_status_code(Payload::RAKP(RAKP::Message2(payload)))?
                    }
                    Payload::RAKP(RAKP::Message4(payload)) => {
                        self.handle_status_code(Payload::RAKP(RAKP::Message4(payload)))?
                    }
                    _ => todo!(),
                }
            }
        } else {
            return Err(IPMIClientError::NoResponse);
        }

        Ok(())
    }

    fn handle_completion_code(&mut self, payload: IpmiPayloadResponse) -> Result<()> {
        match payload.completion_code {
            CompletionCode::CompletedNormally => match payload.command {
                Command::GetChannelAuthCapabilities => {
                    self.handle_channel_auth_capabilities(payload)?
                }
                Command::GetChannelCipherSuites => {
                    while let AuthState::Discovery = self.auth_state {
                        self.cipher_list_index += 1;
                        self.handle_cipher_suites(payload.clone(), self.cipher_list_index)?;
                    }
                }
                _ => todo!(),
            },
            _ => todo!(),
        }
        Ok(())
    }

    fn handle_status_code(&mut self, payload: Payload) -> Result<()> {
        if let Payload::RMCP(RMCPPlusOpenSession::Response(response)) = payload.clone() {
            match response.rmcp_plus_status_code {
                StatusCode::NoErrors => {
                    self.managed_system_session_id =
                        Some(response.managed_system_session_id.clone());
                }
                _ => Err(IPMIClientError::FailedToOpenSession(
                    response.rmcp_plus_status_code,
                ))?,
            }
        }

        if let Payload::RAKP(RAKP::Message2(response)) = payload.clone() {
            match response.rmcp_plus_status_code {
                StatusCode::NoErrors => {
                    self.managed_system_guid = Some(response.managed_system_guid);
                    self.remote_console_session_id = Some(response.remote_console_session_id);
                    self.managed_system_random_number = Some(response.managed_system_random_number);
                    // validate BMC auth code
                    self.validate_rakp2(response)?;
                }
                _ => Err(IPMIClientError::FailedToOpenSession(
                    response.rmcp_plus_status_code,
                ))?,
            }
        }
        if let Payload::RAKP(RAKP::Message4(response)) = payload.clone() {
            match response.rmcp_plus_status_code {
                StatusCode::NoErrors => {
                    // println!("rak4: {:x?}", payload.integrity_check_value.unwrap());
                    let mut rakp4_input_buffer: Vec<u8> = Vec::new();
                    append_u128_to_vec(
                        &mut rakp4_input_buffer,
                        self.remote_console_random_number.clone(),
                    );
                    append_u32_to_vec(
                        &mut rakp4_input_buffer,
                        self.managed_system_session_id.clone().unwrap(),
                    );
                    append_u128_to_vec(
                        &mut rakp4_input_buffer,
                        self.managed_system_guid.clone().unwrap(),
                    );
                    let auth_code =
                        hash_hmac_sha_256(self.sik.clone().unwrap().into(), rakp4_input_buffer);

                    if response.integrity_check_value.clone().unwrap() == auth_code[..16] {
                        // println!("Ses!!!");
                        self.auth_state = AuthState::Established;
                    }
                }
                _ => Err(IPMIClientError::FailedToOpenSession(
                    response.rmcp_plus_status_code,
                ))?,
            }
        }

        Ok(())
    }

    fn create_session_keys(&mut self) -> Result<()> {
        let mut sik_input = Vec::new();
        append_u128_to_vec(&mut sik_input, self.remote_console_random_number);
        append_u128_to_vec(&mut sik_input, self.managed_system_random_number.unwrap());
        sik_input.push(0x14);
        sik_input.push(
            self.username
                .clone()
                .unwrap()
                .len()
                .try_into()
                .map_err(|e| IPMIClientError::UsernameOver255InLength(e))?,
        );
        self.username
            .clone()
            .unwrap()
            .as_bytes()
            .iter()
            .for_each(|char| sik_input.push(char.clone()));

        self.sik = Some(hash_hmac_sha_256(
            self.password_mac_key.clone().unwrap(),
            sik_input,
        ));
        self.k1 = Some(hash_hmac_sha_256(self.sik.unwrap().into(), [1; 20].into()));
        self.k2 = Some(hash_hmac_sha_256(self.sik.unwrap().into(), [2; 20].into()));

        Ok(())
    }

    fn validate_rakp2(&self, response: RAKPMessage2) -> Result<()> {
        let mut rakp2_input_buffer: Vec<u8> = Vec::new();
        append_u32_to_vec(
            &mut rakp2_input_buffer,
            self.remote_console_session_id.unwrap(),
        );
        append_u32_to_vec(
            &mut rakp2_input_buffer,
            self.managed_system_session_id.unwrap(),
        );
        append_u128_to_vec(&mut rakp2_input_buffer, self.remote_console_random_number);
        append_u128_to_vec(
            &mut rakp2_input_buffer,
            self.managed_system_random_number.unwrap(),
        );
        append_u128_to_vec(&mut rakp2_input_buffer, self.managed_system_guid.unwrap());
        rakp2_input_buffer.push(0x14);
        rakp2_input_buffer.push(
            self.username
                .clone()
                .unwrap()
                .len()
                .try_into()
                .map_err(|e| IPMIClientError::UsernameOver255InLength(e))?,
        );
        self.username
            .clone()
            .unwrap()
            .as_bytes()
            .iter()
            .for_each(|char| rakp2_input_buffer.push(char.clone()));

        let manual_auth_code =
            hash_hmac_sha_256(self.password_mac_key.clone().unwrap(), rakp2_input_buffer);
        let mut vec_auth_code = Vec::new();
        vec_auth_code.extend_from_slice(manual_auth_code.as_slice());
        if vec_auth_code != response.key_exchange_auth_code.unwrap() {
            Err(IPMIClientError::FailedToValidateRAKP2)?
        }
        Ok(())
    }

    fn handle_channel_auth_capabilities(&mut self, payload: IpmiPayloadResponse) -> Result<()> {
        let response = GetChannelAuthCapabilitiesResponse::from(payload.data);
        // Currently don't support IPMI v1.5
        if let AuthVersion::IpmiV1_5 = response.auth_version {
            return Err(IPMIClientError::UnsupportedVersion);
        }
        self.channel_auth_capabilities = Some(response);
        Ok(())
    }

    fn handle_cipher_suites(
        &mut self,
        payload: IpmiPayloadResponse,
        cipher_list_index: u8,
    ) -> Result<()> {
        let response = GetChannelCipherSuitesResponse::from(payload.data);
        // update total cipher bytes for the ipmi client object
        if let Some(mut old_bytes) = self.cipher_suite_bytes.clone() {
            response
                .cypher_suite_record_data_bytes
                .iter()
                .for_each(|byte| old_bytes.push(*byte));
            self.cipher_suite_bytes = Some(old_bytes);
        } else {
            self.cipher_suite_bytes = Some(response.cypher_suite_record_data_bytes.clone());
        }

        match response.is_last() {
            false => {
                let cipher_packet = GetChannelCipherSuitesRequest::new(
                    0xe,
                    PayloadType::IPMI,
                    true,
                    cipher_list_index,
                )
                .create_packet();
                self.send_packet(cipher_packet)?;
                Ok(())
            }
            true => {
                // parse through cipher suite records
                self.choose_ciphers();

                // set new state - beginning authentication
                self.auth_state = AuthState::Authentication;
                Ok(())
            }
        }
    }

    fn choose_ciphers(&mut self) {
        let mut priority_map: HashMap<
            u8,
            (
                u8,
                (AuthAlgorithm, IntegrityAlgorithm, ConfidentialityAlgorithm),
            ),
        > = HashMap::new();
        if let Some(bytes) = self.cipher_suite_bytes.clone() {
            bytes.split(|x| *x == 0xc0).for_each(|bytes| {
                if bytes.len() != 4 {
                    return;
                }
                let auth_value: (u8, AuthAlgorithm) = match bytes[1] {
                    0x01 => (2, AuthAlgorithm::RakpHmacSha1),
                    0x02 => (1, AuthAlgorithm::RakpHmacMd5),
                    0x03 => (3, AuthAlgorithm::RakpHmacSha256),
                    _ => (0, AuthAlgorithm::RakpNone),
                };
                let integ_value: (u8, IntegrityAlgorithm) = match bytes[2] {
                    0x41 => (2, IntegrityAlgorithm::HmacSha196),
                    0x42 => (3, IntegrityAlgorithm::HmacMd5128),
                    0x43 => (1, IntegrityAlgorithm::Md5128),
                    0x44 => (4, IntegrityAlgorithm::HmacSha256128),
                    _ => (0, IntegrityAlgorithm::None),
                };
                let confid_value: (u8, ConfidentialityAlgorithm) = match bytes[3] {
                    0x81 => (3, ConfidentialityAlgorithm::AesCbc128),
                    0x82 => (2, ConfidentialityAlgorithm::XRc4128),
                    0x83 => (1, ConfidentialityAlgorithm::XRc440),
                    _ => (0, ConfidentialityAlgorithm::None),
                };
                priority_map.insert(
                    bytes[0],
                    (
                        auth_value.0 + integ_value.0 + confid_value.0,
                        (auth_value.1, integ_value.1, confid_value.1),
                    ),
                );
            });
            let id_to_use = priority_map.iter().max_by_key(|entry| entry.1 .0).unwrap();
            self.auth_algorithm = Some(id_to_use.1 .1 .0.clone());
            self.integrity_algorithm = Some(id_to_use.1 .1 .1.clone());
            self.confidentiality_algorithm = Some(id_to_use.1 .1 .2.clone());
        } else {
            self.auth_algorithm = Some(AuthAlgorithm::RakpNone);
            self.integrity_algorithm = Some(IntegrityAlgorithm::None);
            self.confidentiality_algorithm = Some(ConfidentialityAlgorithm::None);
        }
    }
}

#[derive(Debug, PartialEq)]
enum AuthState {
    Discovery,
    Authentication,
    RAKP1,
    RAKP3,
    Established,
    FailedToEstablish,
}
#[derive(Debug)]

enum CommandState {
    AwaitingResponse,
    ResponseReceived,
}