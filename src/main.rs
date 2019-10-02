use async_std::{
    io, net,
    prelude::*,
    sync::{Arc, RwLock},
    task,
};
use lazy_static::lazy_static;
use rand::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

mod packet;

const MAX_TEST: u64 = 10000;
static player_num: AtomicUsize = AtomicUsize::new(0);
lazy_static! {
    static ref player_map: RwLock<HashMap<u32, Player>> = RwLock::new(HashMap::new());
}

struct Player {
    id: u32,
    position: AtomicU64,
}

impl Player {
    fn get_pos(&self) -> (u32, u32) {
        let pos = self.position.load(Ordering::Relaxed);
        let mask = 0xffffffff;
        let x = (pos >> 32) as u32;
        let y = (pos & mask) as u32;
        (x, y)
    }
    fn set_pos(&self, x: u32, y: u32) {
        let new_pos: u64 = ((x as u64) << 32) | y as u64;
        self.position.store(new_pos, Ordering::SeqCst);
    }
}

fn from_bytes<T>(bytes: &[u8]) -> &T {
    unsafe { &*(bytes.as_ptr() as *const T) }
}

async fn process_packet(packet: &[u8]) {
    use packet::*;
    match SCPacketType::from(packet[1] as usize) {
        SCPacketType::SC_LOGIN_OK => {
            let p = from_bytes::<SCLoginOk>(packet);
            let mut rg = player_map.write().await;
            rg.insert(p.id, Player{id: p.id, position: AtomicU64::new(0)});
        }
        SCPacketType::SC_POS => {
            let p = from_bytes::<SCPosPlayer>(packet);
            let rg = player_map.read().await;
            rg.get(&p.id).expect("Can't find a player by id").set_pos(p.x as u32, p.y as u32);
        }
        _ => {}
    }
}

async fn receiver(stream: Arc<net::TcpStream>) {
    let mut stream = io::BufReader::new(&*stream);
    let mut read_buf = vec![0; 256];
    loop {
        stream
            .read_exact(&mut read_buf[..1])
            .await
            .expect("Can't read from server");
        let total_size = read_buf[0] as usize;
        stream
            .read_exact(&mut read_buf[1..total_size])
            .await
            .expect("Can't read from server");
    }
}

async fn sender(stream: Arc<net::TcpStream>) {
    let mut stream = &*stream;
    let packets = [
        packet::CSMove::up(),
        packet::CSMove::down(),
        packet::CSMove::left(),
        packet::CSMove::right(),
    ];
    let mut rng = StdRng::from_entropy();
    loop {
        let picked_packet = &packets[rng.gen_range(0, packets.len())];
        let bytes = unsafe {
            std::slice::from_raw_parts(
                picked_packet as *const packet::CSMove as *const u8,
                std::mem::size_of::<packet::CSMove>(),
            )
        };
        stream.write_all(bytes).await.expect("Can't send to server");
        task::sleep(Duration::from_millis(1000)).await;
    }
}

fn main() {
    let server = async {
        let handle = (0..MAX_TEST)
            .map(|_| {
                task::spawn(async {
                    let client = net::TcpStream::connect("127.0.0.1:3500")
                        .await
                        .expect("Can't connect to server");
                    let client = Arc::new(client);
                    let recv = task::spawn(receiver(client.clone()));
                    let send = task::spawn(sender(client));
                    let num = player_num.fetch_add(1, Ordering::SeqCst) + 1;
                    println!("Current Player Num: {}", num);
                    recv.await;
                    send.await;
                })
            })
            .last()
            .unwrap();
        handle.await;
    };

    task::block_on(server);
}
