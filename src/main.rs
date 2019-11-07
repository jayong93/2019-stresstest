use async_std::{
    io, net,
    prelude::*,
    sync::{Arc, RwLock},
    task,
};
use ggez::conf::{WindowMode, WindowSetup};
use ggez::event::{self, EventHandler};
use ggez::graphics;
use ggez::graphics::Drawable;
use ggez::{Context, ContextBuilder, GameResult};
use lazy_static::lazy_static;
use rand::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use structopt::StructOpt;

mod packet;

static mut MAX_TEST: u64 = 0;
static mut WINDOW_SIZE: (usize, usize) = (0, 0);
static mut BOARD_SIZE: usize = 0;
static mut CELL_SIZE: f32 = 0.0;
static mut PORT: u16 = 0;
static PLAYER_NUM: AtomicUsize = AtomicUsize::new(0);
lazy_static! {
    static ref PLAYER_MAP: RwLock<HashMap<u32, Player>> =
        RwLock::new(HashMap::with_capacity(unsafe { MAX_TEST } as usize));
}

struct Player {
    _id: u32,
    position: AtomicU64,
}

impl Player {
    fn new(id: u32, x: u32, y: u32) -> Self {
        Player {
            _id: id,
            position: AtomicU64::new(Self::compose_position(x, y)),
        }
    }
    fn compose_position(x: u32, y: u32) -> u64 {
        ((x as u64) << 32) | y as u64
    }
    fn get_pos(&self) -> (u32, u32) {
        let pos = self.position.load(Ordering::Relaxed);
        let mask = 0xffffffff;
        let x = (pos >> 32) as u32;
        let y = (pos & mask) as u32;
        (x, y)
    }
    fn set_pos(&self, x: u32, y: u32) {
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

async fn process_packet(packet: &[u8], my_id: Option<u32>) -> Option<u32> {
    use packet::*;
    match SCPacketType::from(packet[1] as usize) {
        SCPacketType::SC_LOGIN_OK => {
            let p = from_bytes::<SCLoginOk>(packet);
            let mut rg = PLAYER_MAP.write().await;
            rg.insert(p.id, Player::new(p.id, 0, 0));
            Some(p.id)
        }
        SCPacketType::SC_POS => {
            let p = from_bytes::<SCPosPlayer>(packet);
            let id = p.id;

            if let Some(mid) = my_id {
                if mid == id {
                    let rg = PLAYER_MAP.read().await;
                    if let Some(player) = rg.get(&id) {
                        player.set_pos(p.x as u32, p.y as u32);
                    }
                }
            }
            // let rg = PLAYER_MAP.read().await;
            // if let Some(player) = rg.get(&id) {
            //     player.set_pos(p.x as u32, p.y as u32);
            // }
            my_id
        }
        _ => my_id,
    }
}

async fn receiver(stream: Arc<net::TcpStream>) {
    let mut stream = io::BufReader::new(&*stream);
    let mut read_buf = vec![0; 256];
    let mut my_id = None;
    loop {
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
        my_id = process_packet(&read_buf[..total_size], my_id).await;
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
        // stream.write_all(bytes).await.expect("Can't send to server");
        if stream.write_all(bytes).await.is_err() {
            return;
        }
        task::sleep(Duration::from_millis(1000)).await;
    }
}

struct GameState {}

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
            let p_map = task::block_on(PLAYER_MAP.read());
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
        }
        graphics::present(ctx)
    }
}

#[derive(Debug, StructOpt)]
struct CmdOption {
    #[structopt(short, long, default_value = "300")]
    board_size: u16,

    #[structopt(short, long, default_value = "1000")]
    max_player: usize,

    #[structopt(short, long, default_value = "800")]
    window_size: Vec<usize>,

    #[structopt(short, long, default_value = "3500")]
    port: u16,
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
    let server = async {
        let mut handle = None;
        while PLAYER_NUM.load(Ordering::Relaxed) < unsafe { MAX_TEST } as usize {
            handle = (0..)
                .map(|_| {
                    task::spawn(async {
                        if let Ok(client) =
                            net::TcpStream::connect(("127.0.0.1", unsafe { PORT })).await
                        {
                            client.set_nodelay(true).ok();
                            let client = Arc::new(client);
                            let recv = task::spawn(receiver(client.clone()));
                            let send = task::spawn(sender(client));
                            let num = PLAYER_NUM.fetch_add(1, Ordering::Relaxed) + 1;
                            println!("Current Player Num: {}", num);
                            recv.await;
                            send.await;
                        }
                    })
                })
                .take(100)
                .last();
            task::sleep(Duration::from_millis(200)).await;
        }
        handle.unwrap().await;
    };

    let setup = WindowSetup::default().title("Stress Test");
    let win_mode =
        unsafe { WindowMode::default().dimensions(WINDOW_SIZE.0 as f32, WINDOW_SIZE.1 as f32) };
    let cb = ContextBuilder::new("PL-StressTest", "CJY")
        .window_setup(setup)
        .window_mode(win_mode);
    let (mut ctx, mut event_loop) = cb.build()?;
    let mut game_state = GameState {};

    task::spawn(server);
    event::run(&mut ctx, &mut event_loop, &mut game_state)
}
