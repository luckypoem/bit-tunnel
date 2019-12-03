use std::net::{Shutdown};
use async_std::net::TcpStream;
use async_std::sync::{channel, Receiver, Sender};
use async_std::task;
use std::time::Duration;
use async_std::io::{Read, Write};
use async_std::prelude::*;
use std::collections::HashMap;
use super::timer;
use common::protocol::{sc, HEARTBEAT_INTERVAL_MS, VERIFY_DATA, pack_cs_heartbeat, pack_cs_entry_open, pack_cs_connect, pack_cs_connect_domain_name, pack_cs_eof, pack_cs_data, pack_cs_entry_close};
//use common::protocol::{ALIVE_TIMEOUT_TIME_MS};
use log::{info};
use time::{get_time, Timespec};


pub struct Tunnel {
    entry_id_current: u32,
    sender: Sender<Message>,
}


impl Tunnel {
    pub async fn open_entry(&mut self) -> Entry {
        let entry_id = self.entry_id_current;
        self.entry_id_current += 1;
        let (entry_sender, entry_receiver) = channel(999);
        self.sender.send(Message::CS(Cs::EntryOpen(entry_id, entry_sender))).await;

        (
            Entry {
                id: entry_id,
                tunnel_sender: self.sender.clone(),
                entry_receiver,
            }
        )
    }
}


pub struct Entry {
    id: u32,
    tunnel_sender: Sender<Message>,
    entry_receiver: Receiver<EntryMessage>,
}

impl Entry {

    pub async fn write(&self, buf: Vec<u8>) {
        self.tunnel_sender.send(Message::CS(Cs::Data(self.id, buf))).await;
    }

    pub async fn connect_address(&self, address: Vec<u8>) {
        self.tunnel_sender.send(Message::CS(Cs::ConnectIp(self.id, address))).await;
    }

    pub async fn connect_domain_name(&self, domain_name: Vec<u8>, port: u16) {
        self.tunnel_sender
            .send(Message::CS(Cs::ConnectDomainName(self.id, domain_name, port)))
            .await;
    }

    pub async fn eof(&self) {
        self.tunnel_sender.send(Message::CS(Cs::Eof(self.id))).await;
    }

    pub async fn close(&self) {
        self.tunnel_sender.send(Message::CS(Cs::EntryClose(self.id))).await;
    }

    pub async fn read(&self) -> EntryMessage {
        match self.entry_receiver.recv().await {
            Some(msg) => msg,
            None => EntryMessage::Close,
        }
    }
}

pub struct TcpTunnel;

impl TcpTunnel {
    pub fn new(tunnel_id: u32, server_address: String) -> Tunnel {
        let (s, r) = channel(10000);
        let s1 = s.clone();

        task::spawn(async move {
            loop {
                TcpTunnel::task(
                    tunnel_id,
                    server_address.clone(),
                    s.clone(),
                    r.clone(),
                ).await;
            }
        });

        Tunnel {
            entry_id_current: 1,
            sender: s1,
        }
    }

    async fn task(
        tunnel_id: u32,
        server_address: String,
        tunnel_sender: Sender<Message>,
        tunnel_receiver: Receiver<Message>,
    ) {
        let server_stream = match TcpStream::connect(&server_address).await {
            Ok(server_stream) => server_stream,

            Err(_) => {
                task::sleep(Duration::from_millis(1000)).await;
                return;
            }
        };

        let mut entry_map = EntryMap::new();
        let (server_stream0, server_stream1) = &mut (&server_stream, &server_stream);
        let r = async {
            let _ = server_stream_to_tunnel(server_stream0, tunnel_sender.clone()).await;
            let _ = server_stream.shutdown(Shutdown::Both);
        };
        let w = async {
            let _ = tunnel_to_server_stream(tunnel_id, tunnel_receiver.clone(), &mut entry_map, server_stream1).await;
            let _ = server_stream.shutdown(Shutdown::Both);
        };
        let _ = r.join(w).await;

        info!("Tcp tunnel {} broken", tunnel_id);

        for (_, value) in entry_map.iter() {
            value.sender.send(EntryMessage::Close).await;
        }
    }
}


///从server_stream读数据, 向tunnel写数据
async fn server_stream_to_tunnel<R: Read + Unpin>(
    server_stream: &mut R,
    tunnel_sender: Sender<Message>,
) -> std::io::Result<()> {

    loop {
        let mut op = [0u8; 1];
        server_stream.read_exact(&mut op).await?;
        let op = op[0];

        if op == sc::HEARTBEAT {
            tunnel_sender.send(Message::SC(Sc::Heartbeat)).await;
            continue;
        }

        let mut id = [0u8; 4];
        server_stream.read_exact(&mut id).await?;
        let id = u32::from_be(unsafe { *(id.as_ptr() as *const u32) });

        match op {
            sc::ENTRY_CLOSE => {
                tunnel_sender.send(Message::SC(Sc::EntryClose(id))).await;
            }

            sc::EOF => {
                tunnel_sender.send(Message::SC(Sc::Eof(id))).await;
            }

            sc::CONNECT_OK | sc::DATA => {
                let mut len = [0u8; 4];
                server_stream.read_exact(&mut len).await?;
                let len = u32::from_be(unsafe { *(len.as_ptr() as *const u32) });

                let mut buf = vec![0; len as usize];
                server_stream.read_exact(&mut buf).await?;

                let data = buf;

                if op == sc::CONNECT_OK {
                    tunnel_sender.send(Message::SC(Sc::ConnectOk(id, data))).await;
                } else {
                    tunnel_sender.send(Message::SC(Sc::Data(id, data))).await;
                }
            }

            _ => break,
        }
    }

    Ok(())
}

///从channel读数据, 向tunnel写数据
async fn tunnel_to_server_stream<W: Write + Unpin>(
    tunnel_id: u32,
    tunnel_receiver: Receiver<Message>,
    entry_map: &mut EntryMap,
    server_stream: &mut W,
) -> std::io::Result<()> {
    let mut alive_time = get_time();

    let duration = Duration::from_millis(HEARTBEAT_INTERVAL_MS as u64);
    let timer_stream = timer::interval(duration, Message::CS(Cs::Heartbeat));
    let mut msg_stream = timer_stream.merge(tunnel_receiver);

    server_stream.write_all(&VERIFY_DATA).await?;

    loop {
        match msg_stream.next().await {

            Some(msg) => {
                process_tunnel_message(tunnel_id, msg, &mut alive_time, entry_map, server_stream)
                    .await?;
            }

            None => break,
        }
    }

    Ok(())
}

async fn process_tunnel_message<W: Write + Unpin>(
    tid: u32,
    msg: Message,
    alive_time: &mut Timespec,
    entry_map: &mut EntryMap,
    server_stream: &mut W,
) -> std::io::Result<()> {
    match msg {

        Message::CS(cs) => {
            match cs{

                Cs::Heartbeat => {
                    server_stream.write_all(&pack_cs_heartbeat()).await?;
                }

                Cs::EntryOpen(id, entry_sender) => {
                    entry_map.insert(
                        id,
                        EntryInternal {
                            sender: entry_sender,
                            host: String::new(),
                            port: 0,
                        },
                    );

                    server_stream.write_all(&pack_cs_entry_open(id)).await?;
                }

                Cs::ConnectIp(id, buf) => {
                    let data = buf;
                    server_stream.write_all(&pack_cs_connect(id, &data)).await?;
                }

                Cs::ConnectDomainName(id, buf, port) => {
                    let host = String::from_utf8(buf.clone()).unwrap_or(String::new());
                    info!("{}.{}: connecting {}:{}", tid, id, host, port);

                    if let Some(value) = entry_map.get_mut(&id) {
                        value.host = host;
                        value.port = port;
                    }

                    let packed_buffer = pack_cs_connect_domain_name(id, &buf, port);
                    server_stream.write_all(&packed_buffer).await?;
                }

                Cs::Eof(id) => {
                    match entry_map.get(&id) {
                        Some(entry) => {
                            info!(
                                "{}.{}: client shutdown write {}:{}",
                                tid, id, entry.host, entry.port
                            );
                        }

                        None => {
                            info!("{}.{}: client shutdown write unknown server", tid, id);
                        }
                    }

                    server_stream.write_all(&pack_cs_eof(id)).await?;
                }

                Cs::Data(id, buf) => {
                    server_stream.write_all(&pack_cs_data(id, &buf)).await?;
                }

                Cs::EntryClose(id) => {
                    match entry_map.get(&id) {
                        Some(value) => {
                            info!("{}.{}: client close {}:{}", tid, id, value.host, value.port);
                            value.sender.send(EntryMessage::Close).await;
                            server_stream.write_all(&pack_cs_entry_close(id)).await?;
                        }

                        None => {
                            info!("{}.{}: client close unknown server", tid, id);
                        }
                    }

                    entry_map.remove(&id);
                }


            }
        }

        Message::SC(sc) => {
            match sc {
                Sc::Heartbeat => {
                    *alive_time = get_time();
                }

                Sc::EntryClose(id) => {
                    *alive_time = get_time();

                    match entry_map.get(&id) {
                        Some(entry) => {
                            info!("{}.{}: server close {}:{}", tid, id, entry.host, entry.port);

                            entry.sender.send(EntryMessage::Close).await;
                        }

                        None => {
                            info!("{}.{}: server close unknown client", tid, id);
                        }
                    }

                    entry_map.remove(&id);
                }

                Sc::Eof(id) => {
                    *alive_time = get_time();

                    match entry_map.get(&id) {
                        Some(entry) => {
                            info!(
                                "{}.{}: server shutdown write {}:{}",
                                tid, id, entry.host, entry.port
                            );

                            entry.sender.send(EntryMessage::Eof).await;
                        }

                        None => {
                            info!("{}.{}: server shutdown write unknown client", tid, id);
                        }
                    }
                }

                Sc::ConnectOk(id, buf) => {
                    *alive_time = get_time();

                    match entry_map.get(&id) {
                        Some(value) => {
                            info!("{}.{}: connect {}:{} ok", tid, id, value.host, value.port);

                            value.sender.send(EntryMessage::ConnectOk(buf)).await;
                        }

                        None => {
                            info!("{}.{}: connect unknown server ok", tid, id);
                        }
                    }
                }

                //向channel写数据
                Sc::Data(id, buf) => {
                    *alive_time = get_time();
                    if let Some(value) = entry_map.get(&id) {
                        value.sender.send(EntryMessage::Data(buf)).await;
                    };
                }
            }
        }
    }

    Ok(())
}


type EntryMap = HashMap<u32, EntryInternal>;

struct EntryInternal {
    host: String,
    port: u16,
    sender: Sender<EntryMessage>,
}


#[derive(Clone)]
enum Message {
    CS(Cs),
    SC(Sc)
}

#[derive(Clone)]
enum Cs {
    EntryOpen(u32, Sender<EntryMessage>),
    EntryClose(u32),
    ConnectIp(u32, Vec<u8>),
    ConnectDomainName(u32, Vec<u8>, u16),
    Eof(u32),
    Data(u32, Vec<u8>),
    Heartbeat,
}

#[derive(Clone)]
enum Sc{
    EntryClose(u32),
    Eof(u32),
    ConnectOk(u32, Vec<u8>),
    Data(u32, Vec<u8>),
    Heartbeat,
}

pub enum EntryMessage{
    ConnectOk(Vec<u8>),
    Data(Vec<u8>),
    Eof,
    Close,
}
