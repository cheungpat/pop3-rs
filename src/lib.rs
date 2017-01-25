#[cfg(feature = "serde_derive")]
#[macro_use]
extern crate serde_derive;

#[cfg(feature = "serde_derive")]
include!("serde_types.in.rs");

#[cfg(feature = "serde_codegen")]
include!(concat!(env!("OUT_DIR"), "/serde_types.rs"));

/******************************************************************/

#[macro_use]
extern crate log;
#[macro_use]
extern crate error_chain;
#[macro_use]
extern crate lazy_static;
extern crate openssl;
extern crate regex;

use std::io::BufReader;
use openssl::ssl::{SslMethod, SslConnectorBuilder};
use std::net::TcpStream;
use std::path::PathBuf;
use regex::Regex;

pub mod errors {
    error_chain! {
        foreign_links {
            Io(::std::io::Error);
            SslStack(::openssl::error::ErrorStack);
            SslHandshake(::openssl::ssl::HandshakeError<::std::net::TcpStream>);
            UTF8Error(::std::string::FromUtf8Error);
            RegexError(::regex::Error);
        }
    }
}
use errors::*;

mod tcpstream;
mod tcpreader;
mod pop3result;
mod utils;
use tcpstream::TCPStreamType;
use tcpreader::TCPReader;
use pop3result::POP3Data;

#[derive(PartialEq)]
#[derive(Debug)]
enum POP3State {
    BEGIN,
    AUTHORIZATION,
    TRANSACTION,
    UPDATE,
}

pub struct POP3Connection {
    account: AccountConfig,
    stream: TCPStreamType,
    reader: TCPReader,
    state: POP3State,
    timestamp: String,
}

impl POP3Connection {
    pub fn new(account: AccountConfig) -> Result<POP3Connection> {
        trace!("Initiate POP3 Connection");
        let tcp_stream = TcpStream::connect((&account.host[..], account.port))?;
        let (stream, reader) = match account.auth.as_ref() {
            "Plain" => {
                debug!("Creating a Plain TCP Connection");
                let stream = TCPStreamType::Plain(tcp_stream.try_clone()?);
                let reader = TCPReader::Plain(BufReader::new(tcp_stream));
                (stream, reader)
            }
            "SSL" => {
                debug!("Creating a SSL Connection");
                let connector = SslConnectorBuilder::new(SslMethod::tls())?.build();
                let stream = TCPStreamType::SSL(connector.connect(&account.host[..], tcp_stream.try_clone()?)?);
                let reader = TCPReader::SSL(BufReader::new(connector.clone()
                    .connect(&account.host[..], tcp_stream)?));
                (stream, reader)
            }
            _ => return Err("Unknown auth type".into()),
        };

        let mut ctx = POP3Connection {
            account: account,
            stream: stream,
            reader: reader,
            state: POP3State::BEGIN,
            timestamp: String::new(),
        };
        trace!("Connection Established");
        debug!("POP3State::{:?}", ctx.state);
        ctx.read_greeting()?;
        ctx.state = POP3State::AUTHORIZATION;
        debug!("POP3State::{:?}", ctx.state);
        Ok(ctx)
    }

    pub fn login(&mut self) -> Result<POP3Data> {
        assert!(self.state == POP3State::AUTHORIZATION);
        trace!("Attempting to Login");
        let username = &self.account.username.clone();
        let auth_user_cmd = self.send_command("USER", Some(username));
        let auth_response = match auth_user_cmd {
            Ok(_) => {
                debug!("Plain USER/PASS authentication");
                let password = &self.account.password.clone();
                self.send_command("PASS", Some(password))
            }
            Err(_) => {
                debug!("Authenticating using APOP");
                let digest = utils::get_apop_digest(&self.timestamp, &self.account.password);
                let apop_param = &format!("{} {}", self.account.username, digest);
                self.send_command("APOP", Some(apop_param))
            }
        };

        // Switch the current state to TRANSACTION on a successful authentication
        match auth_response {
            Ok(_) => {
                self.state = POP3State::TRANSACTION;
                debug!("POP3State::{:?}", self.state);
                Ok(POP3Data::LOGIN)
            }
            Err(e) => {
                Err(e)
            }
        }
    }

    fn read_greeting(&mut self) -> Result<()> {
        trace!("Reading Greeting from Server");
        let greeting = &self.read_response(false)?[0];
        let re = Regex::new(r"(<.*>)\r\n$")?;
        for cap in re.captures_iter(greeting) {
            self.timestamp = cap[1].to_string();
        }
        Ok(())
    }

    fn send_command(&mut self, command: &str, param: Option<&str>) -> Result<Vec<String>> {
        lazy_static!{
            static ref RESPONSE: Regex = Regex::new(r"^(?P<status>\+OK|-ERR) (?P<statustext>.*)").unwrap();
        }

        // Identify if the command is a multiline command
        let is_multiline = match command {
            "LIST" => param.is_none(),
            _ => false,
        };

        // Create the actual POP3 Command by appending the parameters
        let command = match param {
            Some(x) => format!("{} {}", command, x),
            None => command.to_string()
        };

        info!("C: {}", command);
        self.stream.write_string(&command)?;

        let response = self.read_response(is_multiline)?;
        let status_line = response[0].clone();
        let response_groups = RESPONSE.captures(&status_line).unwrap();
        match response_groups.name("status").ok_or("Regex match failed")?.as_str() {
            "+OK" => Ok(response),
            "-ERR" => Err(response_groups["statustext"].to_string().into()),
            _ => Err("Un-parseable Response".into()),
        }
    }

    fn read_response(&mut self, is_multiline: bool) -> Result<Vec<String>> {
        const LF: u8 = 0x0a;
        let mut response_data: Vec<String> = Vec::new();
        let mut buff = Vec::new();

        //First read the status line
        self.reader.read_until(LF, &mut buff)?;
        response_data.push(String::from_utf8(buff.clone())?);
        info!("S: {}", response_data[0]);

        let mut complete = false;
        while !complete && is_multiline {
            buff.clear();
            self.reader.read_until(LF, &mut buff)?;
            let line = String::from_utf8(buff.clone())?;
            if line == ".\r\n" {
                complete = true;
            } else {
                // Don't add the final .CRLF to the response.
                // It's useless for us
                response_data.push(line);
            }
        }
        Ok(response_data)
    }
}
