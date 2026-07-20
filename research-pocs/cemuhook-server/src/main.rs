//! Minimal Cemuhook/DSU UDP server proof-of-concept.

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::time::interval;

const PORT: u16 = 26760;
const VERSION: u16 = 1001;
const MSG_VERSION: u32 = 0x100000;
const MSG_PORT_INFO: u32 = 0x100001;
const MSG_PAD_DATA: u32 = 0x100002;

fn build_header(msg_type: u32, payload_len: u16, server_id: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(20);
    buf.extend_from_slice(b"DSUS");
    buf.write_u16::<LittleEndian>(VERSION).unwrap();
    buf.write_u16::<LittleEndian>(payload_len).unwrap();
    buf.write_u32::<LittleEndian>(0).unwrap(); // CRC placeholder
    buf.write_u32::<LittleEndian>(server_id).unwrap();
    buf.write_u32::<LittleEndian>(msg_type).unwrap();
    buf
}

fn crc32(data: &mut [u8]) -> u32 {
    data[8] = 0;
    data[9] = 0;
    data[10] = 0;
    data[11] = 0;
    let c = crc32fast::hash(data);
    data[8] = (c & 0xFF) as u8;
    data[9] = ((c >> 8) & 0xFF) as u8;
    data[10] = ((c >> 16) & 0xFF) as u8;
    data[11] = ((c >> 24) & 0xFF) as u8;
    c
}

fn build_version_reply(server_id: u32) -> Vec<u8> {
    let mut h = build_header(MSG_VERSION, 2, server_id);
    h.write_u16::<LittleEndian>(VERSION).unwrap();
    crc32(&mut h);
    h
}

fn build_pad_data(server_id: u32, counter: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(100);
    payload.push(0u8); // slot
    payload.push(2u8); // connected
    payload.push(2u8); // full gyro
    payload.push(2u8); // bluetooth
    payload.extend_from_slice(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06]); // MAC
    payload.push(8u8); // battery full
    payload.push(1u8); // active
    payload.write_u32::<LittleEndian>(counter).unwrap();
    payload.extend_from_slice(&[128u8, 128, 128, 128]); // sticks centered
    payload.extend_from_slice(&[0u8; 28]);
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as u64;
    payload.write_u64::<LittleEndian>(ts).unwrap();
    payload.write_f32::<LittleEndian>(0.0).unwrap();
    payload.write_f32::<LittleEndian>(0.0).unwrap();
    payload.write_f32::<LittleEndian>(1.0).unwrap();
    payload.write_f32::<LittleEndian>(0.0).unwrap();
    payload.write_f32::<LittleEndian>(0.0).unwrap();
    payload.write_f32::<LittleEndian>(0.0).unwrap();

    let mut packet = build_header(MSG_PAD_DATA, payload.len() as u16, server_id);
    packet.extend_from_slice(&payload);
    crc32(&mut packet);
    packet
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let socket = Arc::new(UdpSocket::bind(format!("127.0.0.1:{}", PORT)).await?);
    println!("DSU server on 127.0.0.1:{}", PORT);

    let server_id: u32 = 0x4F584944;
    let clients: Arc<Mutex<HashMap<SocketAddr, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
    let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));

    let recv_sock = socket.clone();
    let recv_clients = clients.clone();
    let recv = tokio::spawn(async move {
        let mut buf = [0u8; 1024];
        loop {
            let (len, src) = match recv_sock.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(_) => continue,
            };
            if len < 20 {
                continue;
            }
            let mut cur = Cursor::new(&buf[..len]);
            let mut magic = [0u8; 4];
            if cur.read_exact(&mut magic).is_err() || &magic != b"DSUC" {
                continue;
            }
            let _ = cur.read_u16::<LittleEndian>();
            let _ = cur.read_u16::<LittleEndian>();
            let _ = cur.read_u32::<LittleEndian>();
            let _ = cur.read_u32::<LittleEndian>();
            let msg_type = cur.read_u32::<LittleEndian>().ok();

            recv_clients.lock().await.insert(src, Instant::now());

            if msg_type == Some(MSG_VERSION) {
                let _ = recv_sock.send_to(&build_version_reply(server_id), src).await;
            }
        }
    });

    let send_sock = socket.clone();
    let send_clients = clients.clone();
    let send_counter = counter.clone();
    let send = tokio::spawn(async move {
        let mut ticker = interval(Duration::from_millis(1000 / 60));
        loop {
            ticker.tick().await;
            let c = {
                let mut c = send_counter.lock().await;
                *c = c.wrapping_add(1);
                *c
            };
            let packet = build_pad_data(server_id, c);
            let addrs: Vec<SocketAddr> = {
                let mut map = send_clients.lock().await;
                map.retain(|_, t| t.elapsed() < Duration::from_secs(5));
                map.keys().copied().collect()
            };
            for addr in addrs {
                let _ = send_sock.send_to(&packet, addr).await;
            }
        }
    });

    let _ = tokio::join!(recv, send);
    Ok(())
}
