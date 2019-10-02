use async_std::{io, net, prelude::*, sync::Arc, task};
use rand::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

mod packet;

const MAX_TEST: u64 = 10000;
static player_num: AtomicUsize = AtomicUsize::new(0);

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
