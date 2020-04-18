use async_std::{
    io::{prelude::*, BufReader},
    net,
};
use ggez::conf::{WindowMode, WindowSetup};
use ggez::event::{self, EventHandler};
use ggez::graphics;
use ggez::graphics::Drawable;
use ggez::{Context, ContextBuilder, GameResult};
use lazy_static::lazy_static;
use rand::prelude::*;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use structopt::StructOpt;
use tokio::{
    sync::{oneshot, RwLock},
    task, time,
};

mod packet;

static mut MAX_TEST: u64 = 0;
static mut WINDOW_SIZE: (usize, usize) = (0, 0);
static mut BOARD_SIZE: usize = 0;
static mut CELL_SIZE: f32 = 0.0;
static mut PORT: u16 = 0;
static PLAYER_NUM: AtomicUsize = AtomicUsize::new(0);
static GLOBAL_DELAY: AtomicUsize = AtomicUsize::new(0);
const DELAY_THRESHOLD: usize = 1000;
lazy_static! {
    static ref PLAYER_MAP: RwLock<HashMap<i32, Player>> =
        RwLock::new(HashMap::with_capacity(unsafe { MAX_TEST } as usize));
}

struct Player {
    _id: i32,
    position: AtomicU32,
    seq_no: Arc<AtomicI32>,
}

impl Player {
    fn new(id: i32, x: i16, y: i16) -> Self {
        Player {
            _id: id,
            position: AtomicU32::new(Self::compose_position(x, y)),
            seq_no: Arc::new(AtomicI32::new(0)),
        }
    }
    fn compose_position(x: i16, y: i16) -> u32 {
        ((x as u32) << 16) | y as u32
    }
    fn get_pos(&self) -> (i16, i16) {
        let pos = self.position.load(Ordering::Relaxed);
        let mask = 0xffffffff;
        let x = (pos >> 16) as i16;
        let y = (pos & mask) as i16;
        (x, y)
    }
    fn set_pos(&self, x: i16, y: i16) {
        let new_pos = Self::compose_position(x, y);
        self.position.store(new_pos, Ordering::SeqCst);
    }
}

fn from_bytes<T>(bytes: &[u8]) -> &T {
    if bytes.len() != std::mem::size_of::<T>() {
        panic!(
            "It's not a compatible type, the bytes have {} bytes, the type is {} bytes",
            bytes.len(),
            std::mem::size_of::<T>()
        );
    }
    unsafe { &*(bytes.as_ptr() as *const T) }
}

async fn process_packet(
    packet: &[u8],
    my_id: &Option<i32>,
    send: Option<oneshot::Sender<Arc<AtomicI32>>>,
) -> Option<i32> {
    use packet::*;
    match SCPacketType::from(packet[1] as usize) {
        SCPacketType::SC_LOGIN_OK => {
            let p = from_bytes::<SCLoginOk>(packet);
            let mut rg = PLAYER_MAP.write().await;
            let player = Player::new(p.id, p.x, p.y);
            if let Some(sender) = send {
                sender.send(player.seq_no.clone()).unwrap();
            }

            rg.insert(p.id, player);
            Some(p.id)
        }
        SCPacketType::SC_POS => {
            let p = from_bytes::<SCPosPlayer>(packet);
            let id = p.id;

            if let Some(mid) = my_id {
                if *mid == id {
                    let rg = PLAYER_MAP.read().await;
                    if let Some(player) = rg.get(&id) {
                        player.set_pos(p.x as i16, p.y as i16);
                        if p.seq_no != player.seq_no.load(Ordering::Relaxed) {
                            let g_delay = GLOBAL_DELAY.load(Ordering::Relaxed);
                            if g_delay < DELAY_THRESHOLD {
                                GLOBAL_DELAY.fetch_add(1, Ordering::Relaxed);
                            } else if g_delay > DELAY_THRESHOLD {
                                GLOBAL_DELAY.fetch_sub(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }
            my_id.clone()
        }
        _ => my_id.clone(),
    }
}

async fn read_packet(
    read_buf: &mut Vec<u8>,
    stream: &mut BufReader<&net::TcpStream>,
    my_id: &mut Option<i32>,
    seq_no_send: Option<oneshot::Sender<Arc<AtomicI32>>>,
) {
    let read_result = stream.read_exact(&mut read_buf[..1]).await;
    if read_result.is_err() {
        return;
    }
    let total_size = read_buf[0] as usize;
    if stream
        .read_exact(&mut read_buf[1..total_size])
        .await
        .is_err()
    {
        return;
    }
    *my_id = process_packet(&read_buf[..total_size], my_id, seq_no_send).await;
}

async fn receiver(stream: Arc<net::TcpStream>, seq_no_send: oneshot::Sender<Arc<AtomicI32>>) {
    let mut stream = BufReader::new(&*stream);
    let mut read_buf = vec![0; 256];
    let mut my_id = None;
    read_packet(&mut read_buf, &mut stream, &mut my_id, Some(seq_no_send)).await;
    loop {
        read_packet(&mut read_buf, &mut stream, &mut my_id, None).await;
    }
}

async fn sender(stream: Arc<net::TcpStream>, seq_no_recv: oneshot::Receiver<Arc<AtomicI32>>) {
    let mut stream = &*stream;
    let player_seq_no = seq_no_recv.await.unwrap();

    let tele_packet = packet::CSTeleport::new();
    let p_bytes = unsafe {
        std::slice::from_raw_parts(
            &tele_packet as *const packet::CSTeleport as *const u8,
            std::mem::size_of::<packet::CSTeleport>(),
        )
    };
    stream.write_all(p_bytes).await.unwrap();

    let packets = [
        packet::CSMove::new(
            packet::Direction::D_UP,
            player_seq_no.fetch_add(1, Ordering::Relaxed) + 1,
        ),
        packet::CSMove::new(
            packet::Direction::D_DOWN,
            player_seq_no.fetch_add(1, Ordering::Relaxed) + 1,
        ),
        packet::CSMove::new(
            packet::Direction::D_LEFT,
            player_seq_no.fetch_add(1, Ordering::Relaxed) + 1,
        ),
        packet::CSMove::new(
            packet::Direction::D_RIGHT,
            player_seq_no.fetch_add(1, Ordering::Relaxed) + 1,
        ),
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
        // stream.write_all(bytes).await.expect("Can't send to server");
        if stream.write_all(bytes).await.is_err() {
            eprintln!("Can't send to the server");
            return;
        }
        time::delay_for(Duration::from_millis(1000)).await;
    }
}

struct GameState {
    executor: tokio::runtime::Runtime,
}

impl EventHandler for GameState {
    fn update(&mut self, ctx: &mut Context) -> GameResult {
        let delta = ggez::timer::delta(ctx).as_secs_f32();
        let target = 1.0 / 30.0;
        if delta <= 1.0 / 30.0 {
            ggez::timer::sleep(std::time::Duration::from_secs_f32(target - delta));
        }
        Ok(())
    }

    fn draw(&mut self, ctx: &mut Context) -> GameResult {
        graphics::clear(ctx, [0.0, 0.0, 0.0, 1.0].into());
        let mut mesh_builder = graphics::MeshBuilder::new();
        let d_mode = graphics::DrawMode::fill();
        let can_render;
        {
            let p_map = self.executor.block_on(PLAYER_MAP.read());
            can_render = !p_map.is_empty();

            for player in p_map.values() {
                let (x, y) = player.get_pos();
                unsafe {
                    let cell_x: f32 = x as f32 * CELL_SIZE + (CELL_SIZE / 4.0);
                    let cell_y: f32 = y as f32 * CELL_SIZE + (CELL_SIZE / 4.0);
                    mesh_builder.rectangle(
                        d_mode,
                        [cell_x, cell_y, CELL_SIZE / 2.0, CELL_SIZE / 2.0].into(),
                        [1.0, 1.0, 1.0, 1.0].into(),
                    );
                }
            }
        }
        if can_render {
            mesh_builder
                .build(ctx)?
                .draw(ctx, graphics::DrawParam::new())?;

            let delay_s = format!("Delay: {} ms", GLOBAL_DELAY.load(Ordering::Relaxed));
            graphics::Text::new(delay_s).draw(ctx, graphics::DrawParam::new())?;
        }
        graphics::present(ctx)
    }
}

#[derive(Debug, StructOpt)]
struct CmdOption {
    #[structopt(short, long, default_value = "400")]
    board_size: u16,

    #[structopt(short, long, default_value = "1000")]
    max_player: usize,

    #[structopt(short, long, default_value = "800")]
    window_size: Vec<usize>,

    #[structopt(short, long, default_value = "9000")]
    port: u16,

    #[structopt(short, long, default_value = "127.0.0.1")]
    ip_addr: String,
}

fn main() -> GameResult {
    let opt = CmdOption::from_args();
    unsafe {
        MAX_TEST = opt.max_player as u64;
        match opt.window_size.len() {
            1 => {
                let size = opt.window_size.first().unwrap();
                WINDOW_SIZE = (*size, *size);
            }
            _ => {
                WINDOW_SIZE = (opt.window_size[0], opt.window_size[1]);
            }
        }
        BOARD_SIZE = opt.board_size as usize;
        PORT = opt.port;
        CELL_SIZE = (WINDOW_SIZE.0 as f32) / (BOARD_SIZE as f32);
    }
    let server = async move {
        let mut handle = None;
        let mut ip_addr =
            std::net::SocketAddrV4::new(FromStr::from_str(opt.ip_addr.as_str()).unwrap(), unsafe {
                PORT
            });
        ip_addr.set_port(unsafe { PORT });

        let mut rng = StdRng::from_entropy();
        while PLAYER_NUM.load(Ordering::Relaxed) < unsafe { MAX_TEST } as usize {
            let mut delay = 50;
            let g_delay = GLOBAL_DELAY.load(Ordering::Relaxed) as u64;

            if g_delay < DELAY_THRESHOLD as u64 {
                if g_delay != 0 {
                    delay = rng.gen_range(0, g_delay);
                }

                if let Ok(mut client) = net::TcpStream::connect(ip_addr).await {
                    handle = Some(task::spawn(async move {
                        client.set_nodelay(true).ok();

                        let login_packet = packet::CSLogin::new();
                        let p_bytes = unsafe {
                            std::slice::from_raw_parts(
                                &login_packet as *const packet::CSLogin as *const u8,
                                std::mem::size_of::<packet::CSLogin>(),
                            )
                        };
                        client.write_all(p_bytes).await.unwrap();

                        let client = Arc::new(client);
                        let (player_send, player_recv) = oneshot::channel();
                        let recv = task::spawn(receiver(client.clone(), player_send));
                        let send = task::spawn(sender(client, player_recv));
                        let num = PLAYER_NUM.fetch_add(1, Ordering::Relaxed) + 1;
                        eprintln!("Current Player Num: {}", num);
                        recv.await.unwrap();
                        send.await.unwrap();
                    }));
                }
            }
            time::delay_for(Duration::from_millis(delay)).await;
        }
        handle.unwrap().await.unwrap();
    };

    let setup = WindowSetup::default().title("Stress Test");
    let win_mode =
        unsafe { WindowMode::default().dimensions(WINDOW_SIZE.0 as f32, WINDOW_SIZE.1 as f32) };
    let cb = ContextBuilder::new("PL-StressTest", "CJY")
        .window_setup(setup)
        .window_mode(win_mode);
    let (mut ctx, mut event_loop) = cb.build()?;
    let mut game_state = GameState {
        executor: tokio::runtime::Builder::new()
            .basic_scheduler()
            .build()
            .expect("Can't build a seq runtime"),
    };

    let runtime = tokio::runtime::Builder::default()
        .threaded_scheduler()
        .enable_all()
        .build()
        .unwrap();
    runtime.spawn(server);
    event::run(&mut ctx, &mut event_loop, &mut game_state)
}
