extern crate alloc;

use embassy_time::{Duration, Timer};
use embedded_io_async::Write;
use log::{debug, error, info, trace};
use serde::Deserialize;

use crate::{CLIENT_UUID, RECONNECT_DELAY_MS};

#[derive(Deserialize)]
struct IncomingCommand<'a> {
    #[serde(rename = "type")]
    cmd_type: &'a str,
    #[serde(default)]
    id: Option<u32>,
    #[serde(default)]
    value: Option<u32>,
}

const SERVER_IP_V4: [u8; 4] = [192, 168, 178, 21]; // Raspberry Pi IP
const SERVER_PORT: u16 = 9000; // TCP server port on the Pi

// Buffers must live at least as long as the TCP socket. Using 'static here is the
// simplest way to ensure the socket does not reference stack-local data.
static mut RX_BUFFER: [u8; 4096] = [0; 4096];
static mut TX_BUFFER: [u8; 4096] = [0; 4096];

pub struct TcpClient<'a> {
    socket: Option<embassy_net::tcp::TcpSocket<'a>>,
    cached_value: u8,
}

impl<'a> TcpClient<'a> {
    pub async fn new() -> Self {
        Self {
            socket: None,
            cached_value: 0,
        }
    }

    pub async fn connect(&mut self, stack: &'a embassy_net::Stack<'a>) {
        #[allow(static_mut_refs)]
        let socket =
            unsafe { embassy_net::tcp::TcpSocket::new(*stack, &mut RX_BUFFER, &mut TX_BUFFER) };

        self.socket = Some(socket);

        if let Some(socket) = self.socket.as_mut() {
            socket.set_timeout(None);
            let address = embassy_net::IpAddress::Ipv4(SERVER_IP_V4.into());
            info!(
                "Connecting to {}.{}.{}.{}:{} ...",
                SERVER_IP_V4[0], SERVER_IP_V4[1], SERVER_IP_V4[2], SERVER_IP_V4[3], SERVER_PORT
            );
            match socket.connect((address, SERVER_PORT)).await {
                Ok(()) => {
                    info!("TCP connected");
                }
                Err(e) => {
                    error!("Connect error: {:?}", e);
                    Timer::after(Duration::from_millis(RECONNECT_DELAY_MS)).await;
                }
            }
        }
    }

    pub async fn serve(&mut self) {
        // Read newline-delimited messages and log them for now.
        let mut line_buf = [0u8; 512];
        let mut line_len: usize = 0;
        let mut chunk = [0u8; 128];

        'read_loop: loop {
            match self.socket.as_mut().unwrap().read(&mut chunk).await {
                Ok(0) => {
                    info!("Server closed connection");
                    break 'read_loop;
                }
                Ok(n) => {
                    trace!("RX chunk ({} bytes): {:02X?}", n, &chunk[..n]);
                    for &b in &chunk[..n] {
                        if b == b'\n' {
                            // process the completed line
                            let line = &line_buf[..line_len];
                            if let Ok(mut s) = core::str::from_utf8(line) {
                                // Trim CR if present
                                s = s.trim_end_matches('\r');
                                if !s.is_empty() {
                                    debug!("RX line: {}", s);
                                    self.handle_line(s).await;
                                }
                            } else {
                                error!("Received non-UTF8 line ({} bytes), ignoring", line_len);
                            }
                            line_len = 0;
                        } else if line_len < line_buf.len() {
                            line_buf[line_len] = b;
                            line_len += 1;
                        } else {
                            // overflow; drop the line
                            error!("Line too long; dropping");
                            line_len = 0;
                        }
                    }
                }
                Err(e) => {
                    error!("Read error: {:?}", e);
                    break 'read_loop;
                }
            }
        }
    }

    pub async fn register(&mut self) {
        let reg = alloc::format!(r#"{{"type":"register","uuid":"{}"}}"#, CLIENT_UUID);
        debug!("TX: {}", reg);
        if let Err(e) = self
            .socket
            .as_mut()
            .unwrap()
            .write_all(reg.as_bytes())
            .await
        {
            error!("Register write error: {:?}", e);
            // reconnect
        }
        if let Err(e) = self.socket.as_mut().unwrap().write_all(b"\n").await {
            error!("Register newline write error: {:?}", e);
        }
        info!("Sent register");
    }

    async fn handle_line(&mut self, s: &str) {
        // Parse with serde-json-core; ignore on failure
        match serde_json_core::de::from_str::<IncomingCommand>(s) {
            Ok((cmd, _rest)) => {
                match cmd.cmd_type {
                    "set_value" => {
                        if let (Some(id), Some(v)) = (cmd.id, cmd.value) {
                            if v <= 100 {
                                self.set_value(id, v).await;
                                return;
                            }

                            // Invalid range
                            let msg = alloc::format!(
                                r#"{{"type":"error","id":{},"message":"value out of range 0..100"}}"#,
                                id
                            );
                            debug!("TX: {}", msg);
                            if let Err(e) = self
                                .socket
                                .as_mut()
                                .unwrap()
                                .write_all(msg.as_bytes())
                                .await
                            {
                                error!("Write error (error set_value id={}): {:?}", id, e);
                            }
                            if let Err(e) = self.socket.as_mut().unwrap().write_all(b"\n").await {
                                error!("Write error (newline error set_value id={}): {:?}", id, e);
                            }
                        } else if let Some(id) = cmd.id {
                            let msg = alloc::format!(
                                r#"{{"type":"error","id":{},"message":"missing value"}}"#,
                                id
                            );
                            debug!("TX: {}", msg);
                            if let Err(e) = self
                                .socket
                                .as_mut()
                                .unwrap()
                                .write_all(msg.as_bytes())
                                .await
                            {
                                error!("Write error (error missing value id={}): {:?}", id, e);
                            }
                            if let Err(e) = self.socket.as_mut().unwrap().write_all(b"\n").await {
                                error!(
                                    "Write error (newline error missing value id={}): {:?}",
                                    id, e
                                );
                            }
                        }
                    }
                    "get_value" => {
                        if let Some(id) = cmd.id {
                            self.get_value(id).await;
                        }
                    }
                    "calibrate" => {
                        if let Some(id) = cmd.id {
                            self.calibrate(id).await;
                        }
                    }
                    _ => {
                        // Ignore unknown types
                    }
                }
            }
            Err(e) => {
                let _ = e; // ignore parse errors; robustness over strictness
            }
        }
    }

    async fn set_value(&mut self, id: u32, v: u32) {
        info!("set_value id={} value={}", id, v);
        self.cached_value = v as u8;
        // Acknowledge success
        let msg = alloc::format!(r#"{{"type":"ack","id":{},"ok":true}}"#, id);
        debug!("TX: {}", msg);
        if let Err(e) = self
            .socket
            .as_mut()
            .unwrap()
            .write_all(msg.as_bytes())
            .await
        {
            error!("Write error (ack set_value id={}): {:?}", id, e);
        }
        if let Err(e) = self.socket.as_mut().unwrap().write_all(b"\n").await {
            error!("Write error (newline ack set_value id={}): {:?}", id, e);
        }
    }

    async fn get_value(&mut self, id: u32) {
        info!("get_value id={} -> {}", id, self.cached_value);
        let msg = alloc::format!(
            r#"{{"type":"value","id":{},"value":{}}}"#,
            id,
            self.cached_value
        );
        debug!("TX: {}", msg);
        if let Err(e) = self
            .socket
            .as_mut()
            .unwrap()
            .write_all(msg.as_bytes())
            .await
        {
            error!("Write error (value id={}): {:?}", id, e);
        }
        if let Err(e) = self.socket.as_mut().unwrap().write_all(b"\n").await {
            error!("Write error (newline value id={}): {:?}", id, e);
        }
    }

    async fn calibrate(&mut self, id: u32) {
        info!("calibrate start (id={})", id);
        self.calibrate_routine().await;
        info!("calibrate done (id={})", id);
        let msg = alloc::format!(r#"{{"type":"ack","id":{},"ok":true}}"#, id);
        debug!("TX: {}", msg);
        if let Err(e) = self
            .socket
            .as_mut()
            .unwrap()
            .write_all(msg.as_bytes())
            .await
        {
            error!("Write error (ack calibrate id={}): {:?}", id, e);
        }
        if let Err(e) = self.socket.as_mut().unwrap().write_all(b"\n").await {
            error!("Write error (newline ack calibrate id={}): {:?}", id, e);
        }
    }

    async fn calibrate_routine(&mut self) {
        // Placeholder: simulate calibration delay
        Timer::after(Duration::from_millis(200)).await;
    }
}
